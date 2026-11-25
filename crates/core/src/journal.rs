//! Ordered, append-only storage for agent run transitions.

use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ModelTurn, RunCheckpoint, RunWorkspace, ToolCall, ToolResult};

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS run_events (
        agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id),
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        event_json TEXT NOT NULL,
        PRIMARY KEY (agent_id, sequence)
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS run_compactions ON run_events(agent_id, sequence)
    WHERE json_extract(event_json, '$.kind') = 'compacted';";

/// A completed storage boundary in the life of one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunEvent {
    /// The initial user prompt.
    RunStarted(String),
    /// The run is about to request its next model turn.
    ModelCallRequested,
    /// A full model response, including usage and provider replay context.
    ModelCallCompleted(ModelTurn),
    /// The tool call is about to be dispatched.
    ToolCallStarted(ToolCall),
    /// The tool returned a result for its call ID.
    ToolCallCompleted(ToolResult),
    /// A skill became available with the loader result in the same transaction.
    SkillLoaded(String),
    /// State at a model-call boundary after transcript compaction.
    Compacted(RunCheckpoint),
    /// The run returned its final answer.
    RunFinished(String),
}

/// A stored event and its one-based position within this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// Position assigned when the transaction committed.
    pub sequence: u64,
    /// The recorded transition.
    pub event: RunEvent,
}

/// A failure to read or extend a run journal.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JournalError {
    /// Another position is already the current tail of this run.
    #[error("expected journal sequence {expected}, found {actual}")]
    Conflict {
        /// Tail supplied by the caller.
        expected: u64,
        /// Last committed sequence, or zero for an empty journal.
        actual: u64,
    },
    /// SQLite could not complete the operation.
    #[error("run journal database: {0}")]
    Database(#[from] rusqlite::Error),
    /// An event could not be encoded or decoded.
    #[error("run journal event: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// Blocking access to one run's journal on the workspace store connection.
#[derive(Debug)]
pub struct RunJournal<'a> {
    connection: &'a mut Connection,
    pub(crate) workspace: RunWorkspace,
}

impl<'a> RunJournal<'a> {
    pub(crate) fn new(connection: &'a mut Connection, workspace: RunWorkspace) -> Self {
        Self {
            connection,
            workspace,
        }
    }

    /// Append an ordered batch atomically and return the new tail.
    ///
    /// Pass zero for an empty journal or the last observed sequence.
    /// An empty batch simply checks the supplied tail.
    ///
    /// # Errors
    ///
    /// A stale tail returns [`JournalError::Conflict`]. A failed batch is rolled back.
    pub fn append(&mut self, expected: u64, events: &[RunEvent]) -> Result<u64, JournalError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut sequence: u64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM run_events WHERE agent_id = ?1",
            [self.workspace.agent_id()],
            |row| row.get(0),
        )?;
        if sequence != expected {
            return Err(JournalError::Conflict {
                expected,
                actual: sequence,
            });
        }
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO run_events(agent_id, sequence, event_json) VALUES (?1, ?2, ?3)",
            )?;
            for event in events {
                sequence += 1;
                insert.execute(params![
                    self.workspace.agent_id(),
                    sequence,
                    serde_json::to_string(event)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(sequence)
    }

    pub(crate) fn recovery_entries(&mut self) -> Result<Vec<JournalEntry>, JournalError> {
        // Read the checkpoint and its tail from a single SQLite snapshot.
        let tx = self.connection.transaction()?;
        let start: u64 = tx.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM run_events
             WHERE agent_id = ?1 AND json_extract(event_json, '$.kind') = 'compacted'",
            [self.workspace.agent_id()],
            |row| row.get(0),
        )?;
        let entries = read_entries(&tx, self.workspace.agent_id(), start.saturating_sub(1))?;
        tx.commit()?;
        Ok(entries)
    }

    /// Load committed entries strictly after the supplied position in order.
    /// Pass zero to read the complete journal.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or a stored event cannot be decoded.
    pub fn load_after(&self, sequence: u64) -> Result<Vec<JournalEntry>, JournalError> {
        read_entries(self.connection, self.workspace.agent_id(), sequence)
    }
}

fn read_entries(
    connection: &Connection,
    agent_id: i64,
    sequence: u64,
) -> Result<Vec<JournalEntry>, JournalError> {
    if sequence >= i64::MAX as u64 {
        return Ok(Vec::new());
    }
    let mut stmt = connection.prepare_cached(
        "SELECT sequence, event_json FROM run_events
             WHERE agent_id = ?1 AND sequence > ?2 ORDER BY sequence",
    )?;
    let mut rows = stmt.query(params![agent_id, sequence])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(JournalEntry {
            sequence: row.get(0)?,
            event: serde_json::from_str(row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?)?,
        });
    }
    Ok(entries)
}
