//! Durable parent-to-worker calls.

use async_trait::async_trait;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;

use crate::journal::append_events;
use crate::{
    JournalError, JsonValue, RunEvent, Tool, ToolContext, ToolError, ToolOutput, ToolRecovery,
    ToolSpec, WaitOn,
};

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS delegated_runs (
        child_agent_id INTEGER PRIMARY KEY REFERENCES workspace_runs(agent_id),
        parent_agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id),
        parent_wait_id TEXT NOT NULL UNIQUE,
        worker TEXT NOT NULL,
        needs_start INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX IF NOT EXISTS ready_delegations ON delegated_runs(child_agent_id)
    WHERE needs_start = 1;";

/// The parent and host-registered worker for a delegated run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DelegatedRun {
    /// The agent ID of the caller awaiting this run.
    pub parent_agent_id: i64,
    /// Worker name used by the host to choose a provider and skill catalog.
    pub worker: String,
}

/// A tool that assigns a task to one worker chosen by the host.
#[derive(Debug, Clone)]
pub struct DelegateTool {
    name: String,
    description: String,
    worker: String,
}

impl DelegateTool {
    /// Bind a local tool name to a worker in the host's scheduler configuration.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        worker: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            worker: worker.into(),
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.name.as_str(),
            self.description.as_str(),
            json!({
                "type": "object",
                "properties": { "task": { "type": "string", "minLength": 1 } },
                "required": ["task"],
                "additionalProperties": false
            }),
        )
        .with_recovery(ToolRecovery::RetryAfterCrash)
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new("delegation requires a journaled run"))
    }

    async fn call_with_output(
        &self,
        _: ToolContext,
        arguments: &JsonValue,
    ) -> Result<ToolOutput, ToolError> {
        let prompt = arguments
            .as_object()
            .filter(|fields| fields.len() == 1)
            .and_then(|fields| fields.get("task"))
            .and_then(JsonValue::as_str)
            .filter(|task| !task.trim().is_empty())
            .ok_or_else(|| ToolError::new("expected a nonempty task string"))?;
        Ok(ToolOutput::Wait(WaitOn::Delegation {
            worker: self.worker.clone(),
            prompt: prompt.to_owned(),
        }))
    }
}

pub(crate) fn start_worker(
    tx: &Transaction<'_>,
    parent: i64,
    wait_id: &str,
    worker: &str,
    prompt: &str,
) -> Result<(), JournalError> {
    // The parent's durable call key also identifies the child within the chat.
    let child = tx.query_row(
        "INSERT INTO workspace_runs(workspace_id, run_key)
         SELECT workspace_id, ?2 FROM workspace_runs WHERE agent_id = ?1
         RETURNING agent_id",
        params![parent, wait_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO delegated_runs(child_agent_id, parent_agent_id, parent_wait_id, worker)
         VALUES (?1, ?2, ?3, ?4)",
        params![child, parent, wait_id, worker],
    )?;
    append_events(tx, child, 0, &[RunEvent::RunStarted(prompt.to_owned())])?;
    Ok(())
}

pub(crate) fn clear_start(tx: &Transaction<'_>, child: i64) -> Result<(), JournalError> {
    tx.execute(
        "UPDATE delegated_runs SET needs_start = 0 WHERE child_agent_id = ?1 AND needs_start = 1",
        [child],
    )?;
    Ok(())
}

pub(crate) fn finish_worker(
    tx: &Transaction<'_>,
    child: i64,
    outcome: Result<&str, &str>,
) -> Result<(), JournalError> {
    let delegation = tx
        .query_row(
            "SELECT parent_agent_id, parent_wait_id, worker
         FROM delegated_runs WHERE child_agent_id = ?1",
            [child],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((parent, wait_id, worker)) = delegation else {
        return Ok(());
    };
    let sequence = tx
        .query_row(
            "SELECT started_sequence FROM run_waits
         WHERE agent_id = ?1 AND wait_id = ?2 AND source = 'delegation' AND resolved = 0",
            params![parent, wait_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(JournalError::InvalidWait(
            "delegated run has no pending parent wait",
        ))?;
    clear_start(tx, child)?;
    let payload = match outcome {
        Ok(answer) => json!({"worker": worker, "answer": answer}),
        Err(error) => json!({"worker": worker, "error": error}),
    };
    append_events(
        tx,
        parent,
        sequence,
        &[RunEvent::Woken { wait_id, payload }],
    )?;
    Ok(())
}
