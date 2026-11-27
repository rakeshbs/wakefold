//! Persistent chat workspaces and their run metadata.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, TransactionBehavior, params};
use thiserror::Error;

use crate::ChatId;

mod migration;

pub(crate) const CHAT_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS chats (
        chat_id BLOB NOT NULL PRIMARY KEY CHECK(length(chat_id) = 16)
    ) WITHOUT ROWID;";

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
        let mut connection = Connection::open(state.join("wakefold.sqlite3"))?;
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
        connection.execute_batch("PRAGMA synchronous = FULL;")?;
        migration::upgrade(&mut connection, &state)?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        connection.execute_batch(CHAT_SCHEMA)?;
        connection.execute_batch(
            "
             CREATE TABLE IF NOT EXISTS workspace_runs (
                 agent_id INTEGER PRIMARY KEY,
                 chat_id BLOB NOT NULL REFERENCES chats(chat_id),
                 run_key TEXT NOT NULL,
                 UNIQUE(chat_id, run_key)
             );",
        )?;
        connection.execute_batch(crate::journal::SCHEMA)?;
        connection.execute_batch(crate::wait::SCHEMA)?;
        connection.execute_batch(crate::delegation::SCHEMA)?;
        crate::mailbox::initialize_schema(&mut connection)?;
        connection.execute_batch(crate::queued_run::SCHEMA)?;
        connection.execute_batch(crate::dynamic::SCHEMA)?;
        connection.execute_batch(crate::artifact::SCHEMA)?;
        Ok(Self { connection, state })
    }

    /// Create a chat and return its UUID.
    ///
    /// The workspace directory is created when the chat's first run opens it.
    ///
    /// # Errors
    /// Returns an error if the chat could not be saved.
    pub fn create_chat(&self) -> Result<ChatId, WorkspaceError> {
        let chat_id = ChatId::new_v4();
        self.connection
            .execute("INSERT INTO chats(chat_id) VALUES (?1)", [chat_id])?;
        Ok(chat_id)
    }

    /// Open the journal for a chat and run, creating their metadata on first use.
    ///
    /// # Errors
    ///
    /// Returns a workspace error if the run cannot be opened.
    pub fn open_journal(
        &mut self,
        chat_id: ChatId,
        run_key: &str,
    ) -> Result<crate::RunJournal<'_>, WorkspaceError> {
        let run = self.open_run_workspace(chat_id, run_key)?;
        let locks = self.state.join("run-locks");
        fs::create_dir_all(&locks)?;
        let lock_path = locks.join(run.agent_id().to_string());
        let artifact_directory = crate::artifact::directory(&self.state);
        Ok(crate::RunJournal::new(
            &mut self.connection,
            run,
            lock_path,
            artifact_directory,
        ))
    }

    /// Open or create a run's workspace using its chat UUID and run key.
    ///
    /// A new run receives its own agent directory under the chat UUID. Reusing
    /// the UUID and run key restores the same directories.
    ///
    /// # Errors
    ///
    /// Returns an error if the mapping or directories cannot be opened.
    /// Retrying with the same identifiers is safe after a partial failure.
    pub fn open_run_workspace(
        &mut self,
        chat_id: ChatId,
        run_key: &str,
    ) -> Result<RunWorkspace, WorkspaceError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let agent_id = reserve_run(&tx, chat_id, run_key)?;
        // Reserve the run durably before creating files. An interrupted
        // initialization can then finish without reassigning the directory.
        tx.commit()?;
        let workspace = RunWorkspace {
            chat_root: chat_directory(&self.state, chat_id),
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

fn chat_directory(state: &Path, chat_id: ChatId) -> PathBuf {
    state.join("workspaces").join(chat_id.to_string())
}

pub(crate) fn reserve_run(
    tx: &rusqlite::Transaction<'_>,
    chat_id: ChatId,
    run_key: &str,
) -> Result<i64, rusqlite::Error> {
    tx.execute(
        "INSERT INTO chats(chat_id) VALUES (?1) ON CONFLICT(chat_id) DO NOTHING",
        [chat_id],
    )?;
    let keys = params![chat_id, run_key];
    tx.execute(
        "INSERT INTO workspace_runs(chat_id, run_key) VALUES (?1, ?2)
         ON CONFLICT(chat_id, run_key) DO NOTHING",
        keys,
    )?;
    tx.query_row(
        "SELECT agent_id FROM workspace_runs WHERE chat_id = ?1 AND run_key = ?2",
        keys,
        |row| row.get(0),
    )
}
