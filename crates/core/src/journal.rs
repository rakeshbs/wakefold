//! Ordered, append-only storage for agent run transitions.

use std::{
    fs::{File, TryLockError},
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    JsonValue, ModelTurn, PendingWait, RunCheckpoint, RunWorkspace, ToolCall, ToolRecovery,
    ToolResult, WorkspaceStore,
};
use crate::{mailbox, wait::record_transition};

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
    /// A tool sent a durable message and receives its message ID as its result.
    MailboxMessageSent {
        /// Stable ID from the sending tool call.
        message_id: String,
        /// Recipient's stable agent ID in this chat.
        to_agent_id: i64,
        /// Message queued for the recipient.
        payload: JsonValue,
    },
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
    /// The host ended the run after an execution failure.
    RunFailed(String),
}

/// A stored event and its one-based position within this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// Position assigned when the transaction committed.
    pub sequence: u64,
    /// The recorded transition.
    pub event: RunEvent,
}

/// A bounded batch of events for a reader following a saved run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunEventPage {
    /// Events strictly after the requested sequence.
    pub entries: Vec<JournalEntry>,
    /// This page has reached the end of a finished or failed run.
    pub complete: bool,
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
    /// The message recipient does not exist in the sender's chat.
    #[error("agent {agent_id} is unavailable in the sender's chat")]
    InvalidMailboxRecipient {
        /// The requested recipient.
        agent_id: i64,
    },
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
    /// A mailbox wait can also append its wake if a matching message is queued.
    ///
    /// # Errors
    ///
    /// A stale tail returns [`JournalError::SequenceMismatch`]. A failed batch is rolled back.
    pub fn append(
        &mut self,
        expected_last_sequence: u64,
        events: &[RunEvent],
    ) -> Result<u64, JournalError> {
        self.append_with_mailbox_wake(expected_last_sequence, events)
            .map(|(sequence, _)| sequence)
    }

    pub(crate) fn append_with_mailbox_wake(
        &mut self,
        expected_last_sequence: u64,
        events: &[RunEvent],
    ) -> Result<(u64, Option<RunEvent>), JournalError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sequence = append_events(
            &tx,
            self.workspace.agent_id(),
            expected_last_sequence,
            events,
        )?;
        let wake = match events.last() {
            Some(RunEvent::WaitStarted(PendingWait {
                on: crate::WaitOn::MailboxMessage { .. },
                ..
            })) => mailbox::deliver_waiting_message(&tx, self.workspace.agent_id())?,
            _ => None,
        };
        let (sequence, wake) = wake.map_or((sequence, None), |entry| {
            (entry.sequence, Some(entry.event))
        });
        tx.commit()?;
        if !events.is_empty()
            && let Some(hook) = self.commit_hook
        {
            if let Some(woken) = &wake {
                let mut committed = events.to_vec();
                committed.push(woken.clone());
                hook(sequence, &committed);
            } else {
                hook(sequence, events);
            }
        }
        Ok((sequence, wake))
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

    /// Record an execution failure for a run, returning whether it changed.
    ///
    /// An empty, finished, failed, or suspended run is left alone. A failed
    /// worker sends the failure back to its parent.
    ///
    /// # Errors
    ///
    /// Returns a journal error if the run is active or the write fails.
    pub fn record_failure(&mut self, error: String) -> Result<bool, JournalError> {
        let _driver_lock = self.lock_driver()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = self.workspace.agent_id();
        let Some((sequence, terminal)) = tail(&tx, id)? else {
            return Ok(false);
        };
        let suspended: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_waits WHERE agent_id = ?1 AND resolved = 0)",
            [id],
            |row| row.get(0),
        )?;
        if terminal || suspended {
            return Ok(false);
        }
        append_events(&tx, id, sequence, &[RunEvent::RunFailed(error)])?;
        tx.commit()?;
        Ok(true)
    }
}

impl WorkspaceStore {
    /// Read a page of one run's journal without opening or creating its workspace.
    ///
    /// Returns `None` if the run has no journal.
    ///
    /// # Errors
    ///
    /// Returns a journal error if the page cannot be read or decoded.
    pub fn run_event_page(
        &self,
        agent_id: i64,
        after: u64,
        limit: NonZeroUsize,
    ) -> Result<Option<RunEventPage>, JournalError> {
        // Read the completion marker and page from the same snapshot so that
        // a finishing run cannot close a stream before its final event is sent.
        let tx = self.connection.unchecked_transaction()?;
        let page = read_run_page(&tx, agent_id, after, limit)?;
        tx.commit()?;
        Ok(page)
    }
}

/// A read-only snapshot for inspecting one or more run journals.
#[derive(Debug)]
pub struct JournalReplay {
    connection: Connection,
}

impl JournalReplay {
    /// Open the saved journal database for replay.
    ///
    /// Reads share one snapshot, including when the daemon is still writing.
    ///
    /// # Errors
    ///
    /// Returns a journal error if the database cannot be opened for reading.
    pub fn open(state_directory: impl AsRef<Path>) -> Result<Self, JournalError> {
        let connection = Connection::open_with_flags(
            state_directory.as_ref().join("wakefold.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.execute_batch("BEGIN")?;
        Ok(Self { connection })
    }

    /// Read events after the requested sequence, or `None` for an unknown run.
    ///
    /// # Errors
    ///
    /// Returns a journal error if the events cannot be read or decoded.
    pub fn run_event_page(
        &self,
        agent_id: i64,
        after: u64,
        limit: NonZeroUsize,
    ) -> Result<Option<RunEventPage>, JournalError> {
        read_run_page(&self.connection, agent_id, after, limit)
    }

    /// Find the worker run linked to a parent's delegation wait.
    ///
    /// # Errors
    ///
    /// Returns a journal error if the relationship cannot be read.
    pub fn worker_id(
        &self,
        parent_agent_id: i64,
        wait_id: &str,
    ) -> Result<Option<i64>, JournalError> {
        Ok(self
            .connection
            .query_row(
                "SELECT child_agent_id FROM delegated_runs
             WHERE parent_agent_id = ?1 AND parent_wait_id = ?2",
                params![parent_agent_id, wait_id],
                |row| row.get(0),
            )
            .optional()?)
    }
}

fn read_run_page(
    connection: &Connection,
    agent_id: i64,
    after: u64,
    limit: NonZeroUsize,
) -> Result<Option<RunEventPage>, JournalError> {
    let Some((last, terminal)) = tail(connection, agent_id)? else {
        return Ok(None);
    };
    let entries = read_entry_page(
        connection,
        agent_id,
        after,
        i64::try_from(limit.get()).unwrap_or(i64::MAX),
    )?;
    let through = entries.last().map_or(after, |entry| entry.sequence);
    Ok(Some(RunEventPage {
        entries,
        complete: terminal && through >= last,
    }))
}

pub(crate) fn tail(
    connection: &Connection,
    agent_id: i64,
) -> Result<Option<(u64, bool)>, JournalError> {
    Ok(connection
        .query_row(
            "SELECT sequence, json_extract(event_json, '$.kind') IN ('run_finished', 'run_failed')
         FROM run_events WHERE agent_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [agent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
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
    read_entry_page(connection, agent_id, sequence, -1)
}

fn read_entry_page(
    connection: &Connection,
    agent_id: i64,
    sequence: u64,
    limit: i64,
) -> Result<Vec<JournalEntry>, JournalError> {
    if sequence >= i64::MAX as u64 {
        return Ok(Vec::new());
    }
    let mut stmt = connection.prepare_cached(
        "SELECT sequence, event_json FROM run_events
             WHERE agent_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![agent_id, sequence, limit])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(JournalEntry {
            sequence: row.get(0)?,
            event: serde_json::from_str(row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?)?,
        });
    }
    Ok(entries)
}
