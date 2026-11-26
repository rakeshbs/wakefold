//! Journal writes and their post-commit diagnostics.

use std::cell::RefCell;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::{JournalError, RunEvent};
use crate::skill::DISPATCHER;
use crate::wait::record_transition;
use crate::{TokenUsage, ToolCallId};

pub(crate) struct JournalTransaction<'a> {
    transaction: Transaction<'a>,
    traces: RefCell<Vec<EventTrace>>,
}

impl<'a> JournalTransaction<'a> {
    pub(crate) fn begin(connection: &'a mut Connection) -> Result<Self, JournalError> {
        Ok(Self {
            transaction: connection.transaction_with_behavior(TransactionBehavior::Immediate)?,
            traces: RefCell::new(Vec::new()),
        })
    }

    pub(crate) fn sql(&self) -> &Transaction<'a> {
        &self.transaction
    }

    pub(crate) fn append(
        &self,
        agent_id: i64,
        expected_last_sequence: u64,
        events: &[RunEvent],
    ) -> Result<u64, JournalError> {
        let mut sequence: u64 = self.transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM run_events WHERE agent_id = ?1",
            [agent_id],
            |row| row.get(0),
        )?;
        if sequence != expected_last_sequence {
            return Err(JournalError::SequenceMismatch {
                expected: expected_last_sequence,
                actual: sequence,
            });
        }
        let mut insert = self.transaction.prepare_cached(
            "INSERT INTO run_events(agent_id, sequence, event_json) VALUES (?1, ?2, ?3)",
        )?;
        for event in events {
            sequence += 1;
            if tracing::enabled!(target: "wakefold::journal", tracing::Level::INFO) {
                self.traces
                    .borrow_mut()
                    .push(EventTrace::new(agent_id, sequence, event));
            }
            record_transition(self, agent_id, sequence, event)?;
            insert.execute(params![agent_id, sequence, serde_json::to_string(event)?])?;
        }
        Ok(sequence)
    }

    pub(crate) fn commit(self) -> Result<(), JournalError> {
        self.transaction.commit()?;
        for trace in self.traces.into_inner() {
            trace.log();
        }
        Ok(())
    }
}

struct EventTrace {
    agent_id: i64,
    sequence: u64,
    kind: &'static str,
    details: TraceDetails,
}

#[derive(Default)]
struct TraceDetails {
    call_id: Option<ToolCallId>,
    tool: Option<String>,
    skill: Option<String>,
    wait_id: Option<String>,
    wait_source: Option<&'static str>,
    message_id: Option<String>,
    to_agent_id: Option<i64>,
    usage: Option<TokenUsage>,
}

impl EventTrace {
    fn new(agent_id: i64, sequence: u64, event: &RunEvent) -> Self {
        let mut details = TraceDetails::default();
        let kind = match event {
            RunEvent::RunStarted(_) => "run_started",
            RunEvent::ModelCallRequested => "model_call_requested",
            RunEvent::ModelCallCompleted(turn) => {
                details.usage = turn.usage;
                "model_call_completed"
            }
            RunEvent::ToolCallPrepared { call, .. } => {
                details.call_id = Some(call.id.clone());
                let tool = if call.name == DISPATCHER {
                    call.arguments
                        .get("tool_name")
                        .or_else(|| call.arguments.get("name"))
                        .and_then(crate::JsonValue::as_str)
                        .unwrap_or(&call.name)
                } else {
                    &call.name
                };
                details.tool = Some(tool.into());
                "tool_call_started"
            }
            RunEvent::ToolCallCompleted(result) => {
                details.call_id = Some(result.call_id.clone());
                "tool_call_completed"
            }
            RunEvent::MailboxMessageSent {
                message_id,
                to_agent_id,
                ..
            } => {
                details.message_id = Some(message_id.clone());
                details.to_agent_id = Some(*to_agent_id);
                "mailbox_message_sent"
            }
            RunEvent::WaitStarted(wait) => {
                details.wait_id = Some(wait.id.clone());
                details.wait_source = Some(wait.on.source());
                "wait_started"
            }
            RunEvent::Woken { wait_id, .. } => {
                details.wait_id = Some(wait_id.clone());
                "woken"
            }
            RunEvent::SkillLoaded(skill) => {
                details.skill = Some(skill.clone());
                "skill_loaded"
            }
            RunEvent::TranscriptCompacted(_) => "compacted",
            RunEvent::RunFinished(_) => "run_finished",
            RunEvent::RunFailed(_) => "run_failed",
        };
        Self {
            agent_id,
            sequence,
            kind,
            details,
        }
    }

    fn log(self) {
        let details = self.details;
        tracing::info!(
            target: "wakefold::journal",
            agent_id = self.agent_id,
            sequence = self.sequence,
            event = self.kind,
            call_id = details.call_id.as_ref().map(ToolCallId::as_str),
            tool = details.tool.as_deref(),
            skill = details.skill.as_deref(),
            wait_id = details.wait_id.as_deref(),
            wait_source = details.wait_source,
            message_id = details.message_id.as_deref(),
            to_agent_id = details.to_agent_id,
            input_tokens = details.usage.map(|usage| usage.input_tokens),
            output_tokens = details.usage.map(|usage| usage.output_tokens),
            "journal event committed"
        );
    }
}
