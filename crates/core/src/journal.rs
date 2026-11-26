//! Ordered, append-only storage for agent run transitions.

use std::{
    fs::{File, TryLockError},
    path::PathBuf,
};

use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::wait::record_transition;
use crate::{
    JsonValue, ModelTurn, PendingWait, RunCheckpoint, RunWorkspace, ToolCall, ToolRecovery,
    ToolResult,
};

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
    /// The run is about to request its next ordinary agent turn.
    /// Provider retries share this event. Compaction summary requests have no request event.
    ModelCallRequested,
    /// The response, usage, and replay data for a completed ordinary agent turn.
    /// A compaction summary contributes usage to the [`RunEvent::TranscriptCompacted`] checkpoint.
    ModelCallCompleted(ModelTurn),
    /// The recovery policy and key were recorded before dispatch.
    ///
    /// The handler may have run if this event has no matching completion.
    #[serde(rename = "tool_call_started")]
    ToolCallPrepared {
        /// The original model request, including its dispatcher arguments.
        call: ToolCall,
        /// The recovery policy declared before the first attempt.
        recovery: ToolRecovery,
        /// Stable key reused by all recovery attempts for this call.
        #[serde(rename = "idempotency_key")]
        tool_call_key: String,
    },
    /// The tool returned a result for its call ID.
    ToolCallCompleted(ToolResult),
    /// The prepared tool call is suspended until its wait resolves.
    WaitStarted(PendingWait),
    /// The wait resolved and its payload becomes the prepared tool call's result.
    Woken {
        /// Identifier from the corresponding [`RunEvent::WaitStarted`].
        wait_id: String,
        /// Data to return to the model as the tool result.
        payload: JsonValue,
    },
    /// A skill became available with the loader result in the same transaction.
    SkillLoaded(String),
    /// State at a model-call boundary after transcript compaction.
    // The SQLite checkpoint index and recovery query use this tag.
    #[serde(rename = "compacted")]
    TranscriptCompacted(RunCheckpoint),
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
    SequenceMismatch {
        /// Tail supplied by the caller.
        expected: u64,
        /// Last committed sequence, or zero for an empty journal.
        actual: u64,
    },
    /// A live driver already owns this run.
    #[error("run {agent_id} is already active")]
    AlreadyRunning {
        /// The stable agent ID of the active run.
        agent_id: i64,
    },
    /// A wait transition conflicts with the durable wait for the run.
    #[error("journal wait transition is invalid: {0}")]
    InvalidWait(&'static str),
    /// The operating system could not open or lock the run's lock file.
    #[error("run lock: {0}")]
    Lock(#[from] std::io::Error),
    /// SQLite could not complete the operation.
    #[error("run journal database: {0}")]
    Database(#[from] rusqlite::Error),
    /// An event could not be encoded or decoded.
    #[error("run journal event JSON: {0}")]
    EventJson(#[from] serde_json::Error),
}

/// Blocking access to one run's journal on the workspace store connection.
#[derive(Debug)]
pub struct RunJournal<'a> {
    connection: &'a mut Connection,
    pub(crate) workspace: RunWorkspace,
    lock_path: PathBuf,
    commit_hook: Option<fn(u64, &[RunEvent])>,
}

impl<'a> RunJournal<'a> {
    pub(crate) fn new(
        connection: &'a mut Connection,
        workspace: RunWorkspace,
        lock_path: PathBuf,
    ) -> Self {
        Self {
            connection,
            workspace,
            lock_path,
            commit_hook: None,
        }
    }

    /// Install a test hook called after each nonempty event batch commits.
    ///
    /// The callback runs before `append` returns, allowing a test to stop a
    /// worker process at an exact durable boundary.
    #[cfg(feature = "test-util")]
    pub fn set_commit_hook(&mut self, hook: fn(u64, &[RunEvent])) {
        self.commit_hook = Some(hook);
    }

    pub(crate) fn lock_driver(&self) -> Result<File, JournalError> {
        // Keep a stable inode across runs. The OS releases its lock when the
        // driver drops this descriptor or the owning process exits.
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => Err(JournalError::AlreadyRunning {
                agent_id: self.workspace.agent_id(),
            }),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// Append an ordered batch atomically and return the new tail.
    ///
    /// Pass zero for an empty journal or the last observed sequence.
    /// An empty batch simply checks the supplied tail.
    ///
    /// # Errors
    ///
    /// A stale tail returns [`JournalError::SequenceMismatch`]. A failed batch is rolled back.
    pub fn append(
        &mut self,
        expected_last_sequence: u64,
        events: &[RunEvent],
    ) -> Result<u64, JournalError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sequence = append_events(
            &tx,
            self.workspace.agent_id(),
            expected_last_sequence,
            events,
        )?;
        tx.commit()?;
        if !events.is_empty()
            && let Some(hook) = self.commit_hook
        {
            hook(sequence, events);
        }
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

pub(crate) fn append_events(
    tx: &rusqlite::Transaction<'_>,
    agent_id: i64,
    expected_last_sequence: u64,
    events: &[RunEvent],
) -> Result<u64, JournalError> {
    let mut sequence: u64 = tx.query_row(
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
    let mut insert = tx.prepare_cached(
        "INSERT INTO run_events(agent_id, sequence, event_json) VALUES (?1, ?2, ?3)",
    )?;
    for event in events {
        sequence += 1;
        record_transition(tx, agent_id, sequence, event)?;
        insert.execute(params![agent_id, sequence, serde_json::to_string(event)?])?;
    }
    Ok(sequence)
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
