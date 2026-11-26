//! Apply run events and resume execution from the latest journal checkpoint.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{Action, RunError, RunOptions, RunUsage, next_action, validate_calls};
use crate::context::Compaction;
use crate::skill::{LOADER, RunSkills};
use crate::{
    JsonValue, Message, ModelTurn, PendingWait, RunEvent, RunJournal, ToolCall, ToolCallId,
    ToolRecovery, ToolResult,
};

/// A durable model-call boundary containing the compacted transcript and run bookkeeping.
///
/// The transcript retains the tool definitions returned by `load_skill`.
/// On resume, the saved skill names identify the handlers to restore from the host's catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunCheckpoint {
    state: RunState,
    #[serde(rename = "loaded_skills")]
    loaded_skill_names: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RunState {
    pub messages: Vec<Message>,
    #[serde(rename = "assistant_context")]
    pub assistant_replay_data: Vec<Option<JsonValue>>,
    used_call_ids: HashSet<ToolCallId>,
    pub model_turns: usize,
    pub usage: RunUsage,
}

impl RunState {
    fn record_model_turn(
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
        self.assistant_replay_data.push(turn.replay_data);
        Ok(())
    }
}

pub(super) enum RunPhase {
    Ready,
    ModelRequested,
    ToolPrepared {
        call: ToolCall,
        recovery: ToolRecovery,
        tool_call_key: String,
    },
    Waiting {
        call_id: ToolCallId,
        wait: PendingWait,
    },
    Finished(String),
}

/// Fresh and resumed runs advance through the same event transitions.
pub(super) struct RunDriver<'a, 'db, 'skills> {
    journal: Option<&'a mut RunJournal<'db>>,
    tail: u64,
    options: &'a RunOptions,
    pub state: RunState,
    pub skills: RunSkills<'skills>,
    pub phase: RunPhase,
}

impl<'a, 'db, 'skills> RunDriver<'a, 'db, 'skills> {
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
            phase: RunPhase::Ready,
        };
        let Some(first) = entries.first() else {
            run.commit([RunEvent::RunStarted(prompt)])?;
            return Ok(run);
        };
        let original_prompt = match &first.event {
            RunEvent::RunStarted(original) => original,
            RunEvent::TranscriptCompacted(snapshot) => {
                let Some(Message::User(original)) = snapshot.state.messages.first() else {
                    return Err(RunError::InvalidJournal(
                        "checkpoint is missing the initial user prompt",
                    ));
                };
                original
            }
            _ => {
                return Err(RunError::InvalidJournal(
                    "expected a run start or checkpoint",
                ));
            }
        };
        if original_prompt != &prompt {
            return Err(RunError::ResumePromptMismatch);
        }
        for entry in entries {
            run.apply(entry.event)?;
            run.tail = entry.sequence;
        }
        if let RunPhase::ToolPrepared { call, recovery, .. } = &run.phase
            && (*recovery != ToolRecovery::RetryAfterCrash
                || run.skills.recovery(call).ok() != Some(ToolRecovery::RetryAfterCrash))
        {
            return Err(RunError::ToolOutcomeUnknown {
                id: call.id.clone(),
                name: run.skills.target_name(call).to_owned(),
            });
        }
        Ok(run)
    }

    pub fn prepare_tool_call(&mut self, call: ToolCall) -> Result<(), RunError> {
        let recovery = self
            .skills
            .recovery(&call)
            .map_err(|source| RunError::Tool {
                name: call.name.clone(),
                source,
            })?;
        self.commit([RunEvent::ToolCallPrepared {
            call,
            recovery,
            tool_call_key: format!("wakefold-{:032x}", rand::random::<u128>()),
        }])
    }

    pub fn tool_call_key(&self) -> Option<&str> {
        match &self.phase {
            RunPhase::ToolPrepared { tool_call_key, .. } => Some(tool_call_key),
            _ => None,
        }
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
            &mut state.assistant_replay_data,
        )?;
        self.commit([RunEvent::TranscriptCompacted(RunCheckpoint {
            state,
            loaded_skill_names: self.skills.loaded().to_vec(),
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
        self.phase = match (&self.phase, event) {
            (RunPhase::Ready, RunEvent::RunStarted(prompt)) if self.state.messages.is_empty() => {
                self.state.messages.push(Message::User(prompt));
                RunPhase::Ready
            }
            (RunPhase::Ready, RunEvent::TranscriptCompacted(snapshot)) => {
                for name in snapshot.loaded_skill_names {
                    self.skills.activate(&name)?;
                }
                self.state = snapshot.state;
                RunPhase::Ready
            }
            (RunPhase::Ready, RunEvent::ModelCallRequested)
                if matches!(action(), Some(Action::CallModel)) =>
            {
                RunPhase::ModelRequested
            }
            (RunPhase::ModelRequested, RunEvent::ModelCallCompleted(turn)) => {
                self.state
                    .record_model_turn(turn, &self.skills, self.options)?;
                RunPhase::Ready
            }
            (
                RunPhase::Ready,
                RunEvent::ToolCallPrepared {
                    call,
                    recovery,
                    tool_call_key,
                },
            ) if !tool_call_key.is_empty()
                && matches!(action(), Some(Action::RunTool(expected)) if expected == &call) =>
            {
                RunPhase::ToolPrepared {
                    call,
                    recovery,
                    tool_call_key,
                }
            }
            (RunPhase::ToolPrepared { call, .. }, RunEvent::SkillLoaded(name))
                if call.name == LOADER && call.arguments["name"].as_str() == Some(&name) =>
            {
                self.skills.activate(&name)?;
                return Ok(());
            }
            (RunPhase::ToolPrepared { call, .. }, RunEvent::ToolCallCompleted(result))
                if call.id == result.call_id =>
            {
                if call.name == LOADER
                    && !call.arguments["name"].as_str().is_some_and(|name| {
                        self.skills.is_loaded(name)
                            && result.value["loaded"].as_str() == Some(name)
                            && result.value["tools"].is_array()
                    })
                {
                    return Err(RunError::InvalidJournal(
                        "loader result does not match the activated skill",
                    ));
                }
                self.state.messages.push(Message::Tool(result));
                RunPhase::Ready
            }
            (
                RunPhase::ToolPrepared {
                    call,
                    tool_call_key,
                    ..
                },
                RunEvent::WaitStarted(wait),
            ) if call.name != LOADER && wait.id == *tool_call_key => RunPhase::Waiting {
                call_id: call.id.clone(),
                wait,
            },
            (RunPhase::Waiting { call_id, wait }, RunEvent::Woken { wait_id, payload })
                if wait.id == wait_id =>
            {
                self.state
                    .messages
                    .push(Message::Tool(ToolResult::new(call_id.clone(), payload)));
                RunPhase::Ready
            }
            (RunPhase::Ready, RunEvent::RunFinished(answer)) if matches!(action(), Some(Action::Finish(expected)) if expected == answer) => {
                RunPhase::Finished(answer)
            }
            _ => return Err(RunError::InvalidJournal("unexpected event at run boundary")),
        };
        Ok(())
    }
}
