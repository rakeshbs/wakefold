//! Persistent chat workspaces and their run metadata.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, TransactionBehavior, params};
use thiserror::Error;

use crate::ChatId;

const SCHEMA_VERSION: i64 = 2;

/// A failure to open workspace metadata or materialize a directory.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// SQLite could not read or persist the workspace mapping.
    #[error("workspace metadata: {0}")]
    Database(#[from] rusqlite::Error),
    /// The database was created by an incompatible version of Wakefold.
    #[error("incompatible Wakefold database in {0}; start with an empty state directory")]
    IncompatibleDatabase(PathBuf),
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

/// A root session as it appears in the saved chat list.
#[derive(Debug, serde::Serialize)]
pub struct SavedChat {
    pub chat_id: ChatId,
    pub root_agent_id: i64,
    /// First user request, shortened for display. Empty chats have no title yet.
    pub title: Option<String>,
}

impl WorkspaceStore {
    /// Read a page of chat roots in descending creation order.
    pub fn saved_chats(
        &self,
        before: Option<i64>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<SavedChat>, WorkspaceError> {
        let mut query = self.connection.prepare(
            "SELECT s.chat_id, s.agent_id,
                (SELECT substr(json_extract(e.event_json, '$.data'), 1, 96)
                 FROM run_events e WHERE e.agent_id=s.agent_id
                 AND json_extract(e.event_json, '$.kind')='run_started'
                 ORDER BY e.sequence LIMIT 1)
             FROM agent_sessions s WHERE s.is_chat_root=1 AND (?1 IS NULL OR s.agent_id < ?1)
             ORDER BY s.agent_id DESC LIMIT ?2",
        )?;
        let rows = query.query_map(params![before, limit.get()], |r| {
            Ok(SavedChat {
                chat_id: r.get(0)?,
                root_agent_id: r.get(1)?,
                title: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

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
        connection.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
        if schema_version(&connection)? != SCHEMA_VERSION {
            initialize_schema(&mut connection, &state)?;
        }
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
        create_session(&self.connection, chat_id, true, None)?;
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
        let (run_id, run) = self.open_workspace(chat_id, run_key)?;
        let locks = self.state.join("run-locks");
        fs::create_dir_all(&locks)?;
        let lock_path = locks.join(run.agent_id().to_string());
        let artifact_directory = crate::artifact::directory(&self.state);
        Ok(crate::RunJournal::new(
            &mut self.connection,
            run_id,
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
        self.open_workspace(chat_id, run_key)
            .map(|(_, workspace)| workspace)
    }

    fn open_workspace(
        &mut self,
        chat_id: ChatId,
        run_key: &str,
    ) -> Result<(i64, RunWorkspace), WorkspaceError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (run_id, agent_id) = if let Some(run) = find_run(&tx, chat_id, run_key)? {
            run
        } else {
            let agent_id = create_session(&tx, chat_id, false, None)?;
            (insert_run(&tx, chat_id, agent_id, run_key)?, agent_id)
        };
        tx.commit()?;
        let workspace = RunWorkspace {
            chat_root: self.state.join("workspaces").join(chat_id.to_string()),
            agent_id,
        };
        fs::create_dir_all(
            workspace
                .chat_root
                .join(workspace.agent_relative_directory()),
        )?;
        Ok((run_id, workspace))
    }
}

fn schema_version(db: &Connection) -> rusqlite::Result<i64> {
    db.pragma_query_value(None, "user_version", |row| row.get(0))
}

fn initialize_schema(db: &mut Connection, state: &Path) -> Result<(), WorkspaceError> {
    // Renaming the old tables rewrites their foreign keys. Validate the rebuilt
    // graph inside the same transaction before re-enabling enforcement.
    db.pragma_update(None, "foreign_keys", false)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match schema_version(&tx)? {
        SCHEMA_VERSION => {}
        1 => {
            tx.execute_batch(include_str!("workspace/upgrade_v1.sql"))?;
            tx.execute_batch(include_str!("workspace/schema.sql"))?;
            tx.execute_batch(include_str!("workspace/copy_v1.sql"))?;
        }
        0 if tx.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*')",
            [],
            |row| row.get::<_, bool>(0),
        )? =>
        {
            tx.execute_batch(include_str!("workspace/schema.sql"))?
        }
        _ => return Err(WorkspaceError::IncompatibleDatabase(state.to_owned())),
    }
    let invalid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
        [],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(WorkspaceError::IncompatibleDatabase(state.to_owned()));
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

pub(crate) fn find_run(
    db: &Connection,
    chat: ChatId,
    key: &str,
) -> rusqlite::Result<Option<(i64, i64)>> {
    use rusqlite::OptionalExtension;
    db.query_row(
        "SELECT run_id, agent_id FROM agent_runs WHERE chat_id=?1 AND run_key=?2",
        params![chat, key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

fn next_id(db: &Connection) -> rusqlite::Result<i64> {
    db.query_row(
        "SELECT MAX((SELECT COALESCE(MAX(agent_id),0) FROM agent_sessions),
                    (SELECT COALESCE(MAX(run_id),0) FROM agent_runs)) + 1",
        [],
        |row| row.get(0),
    )
}

pub(crate) fn ensure_root(db: &Connection, chat: ChatId) -> rusqlite::Result<i64> {
    use rusqlite::OptionalExtension;
    let agent = db
        .query_row(
            "SELECT agent_id FROM agent_sessions WHERE chat_id=?1 AND is_chat_root=1",
            [chat],
            |row| row.get(0),
        )
        .optional()?;
    match agent {
        Some(id) => Ok(id),
        None => create_session(db, chat, true, None),
    }
}

pub(crate) fn create_session(
    db: &Connection,
    chat: ChatId,
    is_root: bool,
    parent: Option<(i64, i64)>,
) -> rusqlite::Result<i64> {
    db.query_row(
        "INSERT INTO agent_sessions(agent_id,chat_id,is_chat_root,parent_agent_id,spawned_by_run_id)
         SELECT MAX((SELECT COALESCE(MAX(agent_id),0) FROM agent_sessions),
                    (SELECT COALESCE(MAX(run_id),0) FROM agent_runs)) + 1, ?1,?2,?3,?4
         RETURNING agent_id",
        params![chat, is_root, parent.map(|p| p.0), parent.map(|p| p.1)],
        |row| row.get(0),
    )
}

pub(crate) fn insert_run(
    db: &Connection,
    chat: ChatId,
    agent: i64,
    key: &str,
) -> rusqlite::Result<i64> {
    let has_run: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_runs WHERE agent_id=?1 OR run_id=?1)",
        [agent],
        |row| row.get(0),
    )?;
    let run = if has_run { next_id(db)? } else { agent };
    db.execute(
        "INSERT INTO agent_runs(run_id,agent_id,chat_id,run_key) VALUES (?1,?2,?3,?4)",
        params![run, agent, chat, key],
    )?;
    Ok(run)
}
