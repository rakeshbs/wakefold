//! Durable conditions that can suspend a tool call.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{JsonValue, Tool, ToolContext, ToolError, ToolOutput, ToolRecovery, ToolSpec};

mod store;
pub(crate) use store::{SCHEMA, record_transition};

/// What must happen before a suspended tool call can return its result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WaitOn {
    /// Resume at or after an absolute deadline.
    Timer {
        /// Deadline according to the system wall clock.
        wake_at: SystemTime,
    },
    /// Resume with a message delivered by the host.
    Message,
    /// Resume with an external event delivered by the host.
    Webhook,
    /// Resume with a human decision delivered by the host.
    Approval,
    /// Wait for the host to approve a specific tool call before dispatch.
    ToolApproval {
        /// Skill-qualified name of the tool.
        tool_name: String,
        /// Actual tool arguments the host reviews.
        arguments: JsonValue,
    },
    /// Start a worker in this chat and resume with its saved answer.
    Delegation {
        /// Worker name registered by the host.
        worker: String,
        /// Task prompt sent to the worker.
        prompt: String,
    },
}

impl WaitOn {
    pub(crate) fn source(&self) -> &'static str {
        match self {
            Self::Timer { .. } => "timer",
            Self::Message => "message",
            Self::Webhook => "webhook",
            Self::Approval => "approval",
            Self::ToolApproval { .. } => "tool_approval",
            Self::Delegation { .. } => "delegation",
        }
    }
}

/// A saved wait for one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PendingWait {
    /// Stable identifier to use when delivering a wake event.
    pub id: String,
    /// The condition the run is waiting for.
    pub on: WaitOn,
}

/// An event the host can deliver to a matching pending wait.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WakeEvent {
    /// A message from another agent.
    Message(JsonValue),
    /// Data from an external webhook.
    Webhook(JsonValue),
    /// An approval decision.
    Approval {
        /// Whether the action was approved.
        approved: bool,
    },
}

impl WakeEvent {
    pub(crate) fn into_result(self) -> (&'static str, JsonValue) {
        match self {
            Self::Message(value) => ("message", value),
            Self::Webhook(value) => ("webhook", value),
            Self::Approval { approved } => ("approval", json!({ "approved": approved })),
        }
    }
}

/// A tool that asks a durable run to wait for a specified number of seconds.
///
/// Add it to a skill such as `timers` to expose `timers.wait` through `call_tool`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TimerWait;

#[async_trait]
impl Tool for TimerWait {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "wait",
            "Pause this run for the given number of seconds, then continue.",
            json!({
                "type": "object",
                "properties": { "seconds": { "type": "integer", "minimum": 0 } },
                "required": ["seconds"],
                "additionalProperties": false
            }),
        )
        .with_recovery(ToolRecovery::RetryAfterCrash)
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new(
            "a timer wait must be handled by a durable run",
        ))
    }

    async fn call_with_output(
        &self,
        _: ToolContext,
        arguments: &JsonValue,
    ) -> Result<ToolOutput, ToolError> {
        let seconds = arguments
            .as_object()
            .filter(|fields| fields.len() == 1)
            .and_then(|fields| fields.get("seconds"))
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| ToolError::new("expected a nonnegative integer seconds field"))?;
        let wake_at = SystemTime::now()
            .checked_add(Duration::from_secs(seconds))
            .filter(|&time| deadline_millis(time).is_some())
            .ok_or_else(|| ToolError::new("timer deadline is out of range"))?;
        Ok(ToolOutput::Wait(WaitOn::Timer { wake_at }))
    }
}

// Round deadlines up and the current time down so a timer never fires early.
pub(crate) fn deadline_millis(deadline: SystemTime) -> Option<i64> {
    let duration = deadline.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_nanos().div_ceil(1_000_000)).ok()
}

pub(crate) fn current_millis(now: SystemTime) -> i64 {
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
