//! Rebuild the driver state from committed transitions before executing new work.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{Action, RunError, RunOptions, RunUsage, next_action, validate_calls};
use crate::skill::{LOADER, RunSkills};
use crate::{CallId, JsonValue, Message, ModelTurn, RunEvent, RunJournal, ToolCall};

/// A durable model-call boundary containing the compacted transcript and run bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunCheckpoint {
    state: RunState,
    loaded_skills: Vec<String>,
}

impl RunCheckpoint {
    pub(super) fn new(state: &RunState, skills: &RunSkills<'_>) -> Self {
        Self {
            state: state.clone(),
            loaded_skills: skills.loaded().to_vec(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RunState {
    pub messages: Vec<Message>,
    pub assistant_context: Vec<Option<JsonValue>>,
    used_call_ids: HashSet<CallId>,
    pub model_turns: usize,
    pub usage: RunUsage,
}

impl RunState {
    fn new(prompt: String) -> Self {
        Self {
            messages: vec![Message::User(prompt)],
            assistant_context: Vec::new(),
            used_call_ids: HashSet::new(),
            model_turns: 0,
            usage: RunUsage::default(),
        }
    }

    pub fn complete_turn(
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

#[derive(Default)]
pub(super) enum Boundary {
    #[default]
    Ready,
    ModelRequested,
    ToolStarted(ToolCall),
    Finished,
}

/// Writes transitions against the last sequence observed by this driver.
pub(super) struct JournalWriter<'a, 'db> {
    journal: Option<&'a mut RunJournal<'db>>,
    tail: u64,
}

impl<'a, 'db> JournalWriter<'a, 'db> {
    pub fn new(journal: Option<&'a mut RunJournal<'db>>) -> Self {
        Self { journal, tail: 0 }
    }

    pub fn record<const N: usize>(
        &mut self,
        events: impl FnOnce() -> [RunEvent; N],
    ) -> Result<(), RunError> {
        if let Some(journal) = &mut self.journal {
            self.tail = journal.append(self.tail, &events())?;
        }
        Ok(())
    }

    pub fn restore(
        &mut self,
        prompt: String,
        skills: &mut RunSkills<'_>,
        options: &RunOptions,
    ) -> Result<(RunState, Boundary), RunError> {
        let mut boundary = Boundary::Ready;
        let Some(journal) = &mut self.journal else {
            return Ok((RunState::new(prompt), boundary));
        };
        let mut entries = journal.recovery_entries()?.into_iter();
        let Some(first) = entries.next() else {
            self.record(|| [RunEvent::RunStarted(prompt.clone())])?;
            return Ok((RunState::new(prompt), boundary));
        };
        self.tail = first.sequence;
        let mut state = match first.event {
            RunEvent::RunStarted(original) if original == prompt => RunState::new(prompt),
            RunEvent::Compacted(snapshot) if matches!(snapshot.state.messages.first(), Some(Message::User(original)) if original == &prompt) =>
            {
                for name in snapshot.loaded_skills {
                    restore_skill(skills, &name)?;
                }
                snapshot.state
            }
            _ => {
                return Err(RunError::InvalidJournal(
                    "initial prompt or checkpoint does not match",
                ));
            }
        };

        for entry in entries {
            self.tail = entry.sequence;
            let action = next_action(&state.messages, usize::MAX);
            boundary = match (&boundary, action, entry.event) {
                (Boundary::Ready, Some(Action::CallModel), RunEvent::ModelCallRequested) => {
                    Boundary::ModelRequested
                }
                (Boundary::ModelRequested, _, RunEvent::ModelCallCompleted(turn)) => {
                    state.complete_turn(turn, skills, options)?;
                    Boundary::Ready
                }
                (
                    Boundary::Ready,
                    Some(Action::RunTool(expected)),
                    RunEvent::ToolCallStarted(call),
                ) if expected == &call => Boundary::ToolStarted(call),
                (Boundary::ToolStarted(call), _, RunEvent::SkillLoaded(name))
                    if call.name == LOADER && call.arguments["name"].as_str() == Some(&name) =>
                {
                    restore_skill(skills, &name)?;
                    continue;
                }
                (Boundary::ToolStarted(call), _, RunEvent::ToolCallCompleted(result))
                    if call.id == result.call_id =>
                {
                    // Earlier journals could contain a completed loader call without
                    // a separate activation event. Its successful result names the skill.
                    if call.name == LOADER {
                        let name = call.arguments["name"]
                            .as_str()
                            .filter(|name| result.value["loaded"].as_str() == Some(*name))
                            .ok_or(RunError::InvalidJournal(
                                "loader result does not match its call",
                            ))?;
                        restore_skill(skills, name)?;
                    }
                    state.messages.push(Message::Tool(result));
                    Boundary::Ready
                }
                (
                    Boundary::Ready,
                    Some(Action::Finish(expected)),
                    RunEvent::RunFinished(answer),
                ) if expected == answer => Boundary::Finished,
                _ => return Err(RunError::InvalidJournal("unexpected event at run boundary")),
            };
        }

        if let Boundary::ToolStarted(call) = &boundary
            && call.name != LOADER
        {
            return Err(RunError::ToolOutcomeUnknown {
                id: call.id.clone(),
                name: call.name.clone(),
            });
        }
        Ok((state, boundary))
    }
}

fn restore_skill(skills: &mut RunSkills<'_>, name: &str) -> Result<(), RunError> {
    skills.activate(name).map_err(|source| RunError::Tool {
        name: LOADER.to_owned(),
        source,
    })
}
