//! Apply run events and resume execution from the latest journal checkpoint.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{Action, RunError, RunOptions, RunUsage, next_action, validate_calls};
use crate::context::Compaction;
use crate::skill::{LOADER, RunSkills};
use crate::{CallId, JsonValue, Message, ModelTurn, RunEvent, RunJournal, ToolCall};

/// A durable model-call boundary containing the compacted transcript and run bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunCheckpoint {
    state: RunState,
    loaded_skills: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RunState {
    pub messages: Vec<Message>,
    pub assistant_context: Vec<Option<JsonValue>>,
    used_call_ids: HashSet<CallId>,
    pub model_turns: usize,
    pub usage: RunUsage,
}

impl RunState {
    fn complete_turn(
        &mut self,
        turn: ModelTurn,
        skills: &RunSkills<'_>,
        options: &RunOptions,
    ) -> Result<(), RunError> {
        let response = turn.response;
        if let crate::ModelResponse::ToolCalls { text, calls } = &response
            && calls.is_empty()
            && text.as_deref().is_none_or(str::is_empty)
        {
            return Err(
                crate::ProviderError::new("provider returned neither tool calls nor text").into(),
            );
        }
        self.usage.record(turn.usage)?;
        self.model_turns += 1;
        if self.model_turns < options.max_model_turns && !response.tool_calls().is_empty() {
            self.usage.check_budget(options.token_budget)?;
            validate_calls(response.tool_calls(), &mut self.used_call_ids, skills)?;
        }
        self.messages.push(Message::Assistant(response));
        self.assistant_context.push(turn.context);
        Ok(())
    }
}

pub(super) enum Boundary {
    Ready,
    ModelRequested,
    ToolStarted(ToolCall),
    Finished(String),
}

/// Fresh and resumed runs advance through the same event transitions.
pub(super) struct Driver<'a, 'db, 'skills> {
    journal: Option<&'a mut RunJournal<'db>>,
    tail: u64,
    options: &'a RunOptions,
    pub state: RunState,
    pub skills: RunSkills<'skills>,
    pub boundary: Boundary,
}

impl<'a, 'db, 'skills> Driver<'a, 'db, 'skills> {
    pub fn open(
        prompt: String,
        mut journal: Option<&'a mut RunJournal<'db>>,
        skills: RunSkills<'skills>,
        options: &'a RunOptions,
    ) -> Result<Self, RunError> {
        let entries = match &mut journal {
            Some(journal) => journal.recovery_entries()?,
            None => Vec::new(),
        };
        let mut run = Self {
            journal,
            tail: 0,
            options,
            state: RunState::default(),
            skills,
            boundary: Boundary::Ready,
        };
        let Some(first) = entries.first() else {
            run.commit([RunEvent::RunStarted(prompt)])?;
            return Ok(run);
        };
        let matches_prompt = match &first.event {
            RunEvent::RunStarted(original) => original == &prompt,
            RunEvent::Compacted(snapshot) => {
                matches!(snapshot.state.messages.first(), Some(Message::User(original)) if original == &prompt)
            }
            _ => false,
        };
        if !matches_prompt {
            return Err(RunError::InvalidJournal(
                "initial prompt or checkpoint does not match",
            ));
        }
        for entry in entries {
            run.apply(entry.event)?;
            run.tail = entry.sequence;
        }
        if let Boundary::ToolStarted(call) = &run.boundary
            && call.name != LOADER
        {
            return Err(RunError::ToolOutcomeUnknown {
                id: call.id.clone(),
                name: call.name.clone(),
            });
        }
        Ok(run)
    }

    pub fn compact(
        &mut self,
        compaction: Compaction<'_>,
        summary: ModelTurn,
    ) -> Result<(), RunError> {
        // Move the transcript into the checkpoint so compaction needs no second copy.
        let mut state = std::mem::take(&mut self.state);
        state.usage.record(summary.usage)?;
        compaction.apply(
            summary.response,
            &mut state.messages,
            &mut state.assistant_context,
        )?;
        self.commit([RunEvent::Compacted(RunCheckpoint {
            state,
            loaded_skills: self.skills.loaded().to_vec(),
        })])
    }

    pub fn commit(
        &mut self,
        events: impl AsRef<[RunEvent]> + IntoIterator<Item = RunEvent>,
    ) -> Result<(), RunError> {
        if let Some(journal) = &mut self.journal {
            self.tail = journal.append(self.tail, events.as_ref())?;
        }
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }

    fn apply(&mut self, event: RunEvent) -> Result<(), RunError> {
        let action = || next_action(&self.state.messages, usize::MAX);
        self.boundary = match (&self.boundary, event) {
            (Boundary::Ready, RunEvent::RunStarted(prompt)) if self.state.messages.is_empty() => {
                self.state.messages.push(Message::User(prompt));
                Boundary::Ready
            }
            (Boundary::Ready, RunEvent::Compacted(snapshot)) => {
                for name in snapshot.loaded_skills {
                    activate_skill(&mut self.skills, &name)?;
                }
                self.state = snapshot.state;
                Boundary::Ready
            }
            (Boundary::Ready, RunEvent::ModelCallRequested)
                if matches!(action(), Some(Action::CallModel)) =>
            {
                Boundary::ModelRequested
            }
            (Boundary::ModelRequested, RunEvent::ModelCallCompleted(turn)) => {
                self.state.complete_turn(turn, &self.skills, self.options)?;
                Boundary::Ready
            }
            (Boundary::Ready, RunEvent::ToolCallStarted(call)) if matches!(action(), Some(Action::RunTool(expected)) if expected == &call) => {
                Boundary::ToolStarted(call)
            }
            (Boundary::ToolStarted(call), RunEvent::SkillLoaded(name))
                if call.name == LOADER && call.arguments["name"].as_str() == Some(&name) =>
            {
                activate_skill(&mut self.skills, &name)?;
                return Ok(());
            }
            (Boundary::ToolStarted(call), RunEvent::ToolCallCompleted(result))
                if call.id == result.call_id =>
            {
                // Earlier journals recorded successful loader calls without a SkillLoaded event.
                if call.name == LOADER {
                    let name = call.arguments["name"]
                        .as_str()
                        .filter(|name| result.value["loaded"].as_str() == Some(*name))
                        .ok_or(RunError::InvalidJournal(
                            "loader result does not match its call",
                        ))?;
                    activate_skill(&mut self.skills, name)?;
                }
                self.state.messages.push(Message::Tool(result));
                Boundary::Ready
            }
            (Boundary::Ready, RunEvent::RunFinished(answer)) if matches!(action(), Some(Action::Finish(expected)) if expected == answer) => {
                Boundary::Finished(answer)
            }
            _ => return Err(RunError::InvalidJournal("unexpected event at run boundary")),
        };
        Ok(())
    }
}

fn activate_skill(skills: &mut RunSkills<'_>, name: &str) -> Result<(), RunError> {
    skills.activate(name).map_err(|source| RunError::Tool {
        name: LOADER.to_owned(),
        source,
    })
}
