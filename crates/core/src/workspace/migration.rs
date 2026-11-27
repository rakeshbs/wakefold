//! Upgrade workspaces that used separate chat keys and numeric directories.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{CHAT_SCHEMA, ChatId, WorkspaceError, chat_directory};

pub(super) fn upgrade(connection: &mut Connection, state: &Path) -> Result<(), WorkspaceError> {
    if !has_table(connection, "workspaces")? {
        return Ok(());
    }

    // SQLite requires foreign keys to be disabled while replacing referenced
    // tables. Validate the rebuilt schema before committing it.
    connection.pragma_update(None, "foreign_keys", false)?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !has_table(&tx, "workspaces")? {
        return Ok(());
    }
    let legacy = {
        let mut query = tx.prepare("SELECT id, chat_key FROM workspaces ORDER BY id")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let ids = migration_ids(state, &legacy)?;
    #[cfg(unix)]
    fs::File::open(state)?.sync_all()?;
    tx.execute_batch(CHAT_SCHEMA)?;
    tx.execute_batch(
        "CREATE TEMP TABLE migrated_chats (old_id INTEGER PRIMARY KEY, chat_id BLOB NOT NULL);",
    )?;
    for (old_id, key) in &legacy {
        let chat_id = ids.get(key).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "missing chat ID in migration map",
            )
        })?;
        tx.execute("INSERT INTO chats(chat_id) VALUES (?1)", [chat_id])?;
        tx.execute(
            "INSERT INTO migrated_chats VALUES (?1, ?2)",
            params![old_id, chat_id],
        )?;
    }
    tx.execute_batch(
        "CREATE TABLE uuid_workspace_runs (
             agent_id INTEGER PRIMARY KEY,
             chat_id BLOB NOT NULL REFERENCES chats(chat_id),
             run_key TEXT NOT NULL,
             UNIQUE(chat_id, run_key)
         );
         INSERT INTO uuid_workspace_runs
             SELECT agent_id, chat_id, run_key FROM workspace_runs
             LEFT JOIN migrated_chats ON old_id = workspace_id;
         DROP TABLE workspace_runs;
         ALTER TABLE uuid_workspace_runs RENAME TO workspace_runs;",
    )?;
    if has_table(&tx, "artifacts")? {
        tx.execute_batch(
            "CREATE TABLE uuid_artifacts (
                 id INTEGER PRIMARY KEY,
                 chat_id BLOB NOT NULL REFERENCES chats(chat_id)
             );
             INSERT INTO uuid_artifacts
                 SELECT id, chat_id FROM artifacts
                 LEFT JOIN migrated_chats ON old_id = workspace_id;
             DROP TABLE artifacts;
             ALTER TABLE uuid_artifacts RENAME TO artifacts;",
        )?;
    }
    tx.execute_batch("DROP TABLE workspaces; DROP TABLE migrated_chats;")?;
    let dangling = tx
        .query_row(
            "SELECT \"table\" FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(table) = dangling {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("chat ID migration found an invalid reference in {table}"),
        )
        .into());
    }

    for (old_id, key) in legacy {
        move_workspace(state, old_id, ids[&key])?;
    }
    #[cfg(unix)]
    {
        let workspaces = state.join("workspaces");
        if workspaces.try_exists()? {
            fs::File::open(workspaces)?.sync_all()?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn has_table(connection: &Connection, name: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )
}

fn migration_ids(state: &Path, legacy: &[(i64, String)]) -> io::Result<BTreeMap<String, ChatId>> {
    let path = state.join("chat-id-migration.json");
    match fs::File::open(&path) {
        Ok(file) => return Ok(serde_json::from_reader(io::BufReader::new(file))?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut ids = BTreeMap::new();
    let mut assigned = BTreeSet::new();
    // Keep existing UUIDs when possible, including clients that supplied them
    // before the API required UUIDs.
    for (_, key) in legacy {
        if let Ok(chat_id) = key.parse::<ChatId>()
            && assigned.insert(chat_id)
        {
            ids.insert(key.clone(), chat_id);
        }
    }
    for (_, key) in legacy {
        ids.entry(key.clone()).or_insert_with(ChatId::new_v4);
    }

    // Save the mapping before moving any files. If the process stops before
    // SQLite commits, the next startup can finish with the same UUIDs.
    let mut file = tempfile::NamedTempFile::new_in(state)?;
    serde_json::to_writer_pretty(file.as_file_mut(), &ids)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(ids)
}

fn move_workspace(state: &Path, old_id: i64, chat_id: ChatId) -> io::Result<()> {
    let old = state.join("workspaces").join(old_id.to_string());
    let new = chat_directory(state, chat_id);
    match old.symlink_metadata() {
        // An earlier attempt moved it, or the run was queued and had no files.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    match new.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(old, new),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("both workspace directories exist for chat {chat_id}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::{RunEvent, WorkspaceStore, journal, queued_run};

    #[test]
    fn interrupted_upgrade_reuses_chat_ids_and_preserves_saved_runs() {
        let state = tempfile::tempdir().unwrap();
        let root = state.path();
        let existing_id = ChatId::new_v4();
        let existing_key = existing_id.simple().to_string();
        let db = Connection::open(root.join("wakefold.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE workspaces (id INTEGER PRIMARY KEY, chat_key TEXT NOT NULL UNIQUE);
             CREATE TABLE workspace_runs (
                 agent_id INTEGER PRIMARY KEY,
                 workspace_id INTEGER NOT NULL REFERENCES workspaces(id),
                 run_key TEXT NOT NULL,
                 UNIQUE(workspace_id, run_key)
             );
             CREATE TABLE artifacts (
                 id INTEGER PRIMARY KEY, workspace_id INTEGER NOT NULL REFERENCES workspaces(id)
             );
             CREATE TABLE artifact_revisions (
                 artifact_id INTEGER NOT NULL REFERENCES artifacts(id),
                 revision INTEGER NOT NULL, title TEXT NOT NULL, filename TEXT NOT NULL,
                 media_type TEXT NOT NULL, size_bytes INTEGER NOT NULL,
                 agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id), tool_call_key TEXT NOT NULL,
                 PRIMARY KEY (artifact_id, revision)
             );
             INSERT INTO workspaces VALUES (7, 'old-chat');
             INSERT INTO workspace_runs VALUES (12, 7, 'run');
             INSERT INTO artifacts VALUES (22, 7);
             INSERT INTO artifact_revisions VALUES
                 (22, 1, 'Note', 'note.txt', 'text/plain', 5, 12, 'call');",
        )
        .unwrap();
        db.execute("INSERT INTO workspaces VALUES (8, ?1)", [&existing_key])
            .unwrap();
        db.execute_batch(journal::SCHEMA).unwrap();
        db.execute_batch(queued_run::SCHEMA).unwrap();
        let start = RunEvent::RunStarted("draft a note".into());
        db.execute(
            "INSERT INTO run_events VALUES (12, 1, ?1)",
            [serde_json::to_string(&start).unwrap()],
        )
        .unwrap();
        db.execute("INSERT INTO queued_runs VALUES (12, 'assistant', 1)", [])
            .unwrap();
        drop(db);
        let old = root.join("workspaces/7/agents/12");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("note.txt"), "draft").unwrap();
        let copies = crate::artifact::directory(root);
        fs::create_dir(&copies).unwrap();
        fs::write(copies.join("12-call"), "hello").unwrap();
        fs::create_dir(root.join("workspaces/8")).unwrap();
        let collision = chat_directory(root, existing_id);
        fs::create_dir(&collision).unwrap();

        // Force a failure after the first directory is moved but before the
        // SQLite schema commits.
        assert!(WorkspaceStore::open(root).is_err());
        let ids = migration_ids(root, &[]).unwrap();
        let chat_id = ids["old-chat"];
        assert_eq!(ids[&existing_key], existing_id);
        assert!(chat_directory(root, chat_id).exists());
        assert!(!old.exists());
        fs::remove_dir(collision).unwrap();

        let mut store = WorkspaceStore::open(root).unwrap();
        let run = store.open_run_workspace(chat_id, "run").unwrap();
        assert_eq!(run.agent_id(), 12);
        assert_eq!(
            run.chat_root(),
            chat_directory(&fs::canonicalize(root).unwrap(), chat_id)
        );
        assert_eq!(
            fs::read_to_string(run.chat_root().join("agents/12/note.txt")).unwrap(),
            "draft"
        );
        assert_eq!(
            store
                .queue_run(chat_id, "run", "assistant", "draft a note")
                .unwrap(),
            12
        );
        let entries = store
            .open_journal(chat_id, "run")
            .unwrap()
            .load_after(0)
            .unwrap();
        assert_eq!(entries[0].event, start);
        assert_eq!(store.artifacts(chat_id).unwrap()[0].artifact_id, 22);
        let (_, mut file) = store.open_artifact(chat_id, 22, 1).unwrap().unwrap();
        let mut published = String::new();
        file.read_to_string(&mut published).unwrap();
        assert_eq!(published, "hello");
        assert!(store.artifacts(existing_id).unwrap().is_empty());
        drop(store);

        let mut store = WorkspaceStore::open(root).unwrap();
        assert_eq!(store.open_run_workspace(chat_id, "run").unwrap(), run);
    }
}
