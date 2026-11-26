//! Persistent chat workspaces and their run metadata.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, TransactionBehavior, params};
use thiserror::Error;

/// A failure to open workspace metadata or materialize a directory.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// SQLite could not read or persist the workspace mapping.
    #[error("workspace metadata: {0}")]
    Database(#[from] rusqlite::Error),
    /// A workspace directory could not be accessed.
    #[error("workspace filesystem: {0}")]
    Filesystem(#[from] std::io::Error),
}

/// Native paths available to trusted host tools for one agent run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunWorkspace {
    chat_root: PathBuf,
    agent_id: i64,
}

impl RunWorkspace {
    /// The shared chat directory against which artifact paths are resolved.
    pub fn chat_root(&self) -> &Path {
        &self.chat_root
    }

    /// The stable ID assigned to this run's agent by the store.
    pub fn agent_id(&self) -> i64 {
        self.agent_id
    }

    /// The agent's directory relative to the chat root.
    pub fn agent_relative_directory(&self) -> PathBuf {
        Path::new("agents").join(self.agent_id.to_string())
    }
}

/// A blocking SQLite store. Open workspaces before entering the async agent loop.
#[derive(Debug)]
pub struct WorkspaceStore {
    pub(crate) connection: Connection,
    pub(crate) state: PathBuf,
}

impl WorkspaceStore {
    /// Open the runtime state directory, initializing its metadata if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory or database cannot be initialized.
    pub fn open(state_directory: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let state = state_directory.as_ref();
        fs::create_dir_all(state)?;
        let state = fs::canonicalize(state)?;
        let connection = Connection::open(state.join("wakefold.sqlite3"))?;
        // Concurrent first opens can return SQLITE_BUSY immediately while
        // upgrading the journal mode, even with SQLite's busy handler enabled.
        let started = Instant::now();
        loop {
            match connection.execute_batch("PRAGMA journal_mode = WAL") {
                Err(error)
                    if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                        && started.elapsed() < Duration::from_secs(1) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => {
                    result?;
                    break;
                }
            }
        }
        connection.execute_batch(
            "PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS workspaces (
                 id INTEGER PRIMARY KEY,
                 chat_key TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS workspace_runs (
                 agent_id INTEGER PRIMARY KEY,
                 workspace_id INTEGER NOT NULL REFERENCES workspaces(id),
                 run_key TEXT NOT NULL,
                 UNIQUE(workspace_id, run_key)
             );",
        )?;
        connection.execute_batch(crate::journal::SCHEMA)?;
        connection.execute_batch(crate::wait::SCHEMA)?;
        Ok(Self { connection, state })
    }

    /// Open the journal for a chat and run, creating their metadata on first use.
    ///
    /// # Errors
    ///
    /// Returns a workspace error if the run cannot be opened.
    pub fn open_journal(
        &mut self,
        chat_key: &str,
        run_key: &str,
    ) -> Result<crate::RunJournal<'_>, WorkspaceError> {
        let run = self.open_run_workspace(chat_key, run_key)?;
        let locks = self.state.join("run-locks");
        fs::create_dir_all(&locks)?;
        let lock_path = locks.join(run.agent_id().to_string());
        Ok(crate::RunJournal::new(&mut self.connection, run, lock_path))
    }

    /// Open or create a run's workspace using stable chat and run keys.
    ///
    /// A new run receives its own agent directory. Reusing both keys restores
    /// the same directories. Only database-generated IDs appear in host paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the mapping or directories cannot be opened.
    /// Retrying with the same keys is safe after a partial failure.
    pub fn open_run_workspace(
        &mut self,
        chat_key: &str,
        run_key: &str,
    ) -> Result<RunWorkspace, WorkspaceError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO workspaces(chat_key) VALUES (?1) ON CONFLICT(chat_key) DO NOTHING",
            [chat_key],
        )?;
        let workspace_id: i64 = tx.query_row(
            "SELECT id FROM workspaces WHERE chat_key = ?1",
            [chat_key],
            |row| row.get(0),
        )?;
        let keys = params![workspace_id, run_key];
        tx.execute(
            "INSERT INTO workspace_runs(workspace_id, run_key) VALUES (?1, ?2)
             ON CONFLICT(workspace_id, run_key) DO NOTHING",
            keys,
        )?;
        let agent_id = tx.query_row(
            "SELECT agent_id FROM workspace_runs WHERE workspace_id = ?1 AND run_key = ?2",
            keys,
            |row| row.get(0),
        )?;
        // Reserve both IDs durably before creating files. An interrupted
        // initialization can then finish without reassigning the directory.
        tx.commit()?;
        let workspace = RunWorkspace {
            chat_root: self.state.join("workspaces").join(workspace_id.to_string()),
            agent_id,
        };
        fs::create_dir_all(
            workspace
                .chat_root
                .join(workspace.agent_relative_directory()),
        )?;
        Ok(workspace)
    }
}
