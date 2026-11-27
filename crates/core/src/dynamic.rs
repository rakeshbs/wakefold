//! Durable admission of roots and independently scheduled subagents.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::journal::JournalTransaction;
use crate::{
    ChatId, JournalError, JsonValue, RunEvent, Tool, ToolContext, ToolError, ToolGrant, ToolOutput,
    ToolRecovery, ToolSpec, WorkspaceStore,
};

/// A failure to admit a run to the scheduler.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum QueueRunError {
    /// The chat ID and run key belong to a different request.
    #[error("this chat ID and run key already belong to a different request")]
    Conflict,
    /// The agent already has an unfinished request.
    #[error("the conversation has an unfinished run")]
    Busy,
    /// The journal could not save or verify the request.
    #[error(transparent)]
    Journal(#[from] JournalError),
}

impl From<rusqlite::Error> for QueueRunError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Journal(error.into())
    }
}

/// Resolved configuration saved when admitting a root and inherited by its children.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    pub model: String,
    #[serde(default)]
    pub instructions: String,
    pub max_model_turns: NonZeroUsize,
    pub token_budget: u64,
    #[serde(default)]
    pub context_byte_budget: Option<NonZeroUsize>,
    /// Installed skills and their descriptions, in startup order.
    #[serde(default)]
    pub catalog: BTreeMap<String, String>,
    #[serde(default)]
    pub tool_grants: BTreeMap<String, ToolGrant>,
}

impl WorkspaceStore {
    /// Admit a request for the chat's root agent and return its stable agent ID.
    /// Use [`Self::queue_chat_turn`] to get the request's ID as well.
    pub fn queue_root(
        &mut self,
        chat_id: ChatId,
        run_key: &str,
        task: &str,
        settings: &AgentSettings,
        time_zone: Option<chrono_tz::Tz>,
    ) -> Result<i64, QueueRunError> {
        self.queue_chat_turn(chat_id, run_key, task, settings, time_zone)
            .map(|(agent_id, _)| agent_id)
    }

    /// Append a user request to this chat, returning the stable agent ID and the new run ID.
    ///
    /// Retrying the same key and prompt returns the saved IDs. A new request
    /// requires the preceding run to be completed or failed.
    pub fn queue_chat_turn(
        &mut self,
        chat_id: ChatId,
        run_key: &str,
        task: &str,
        settings: &AgentSettings,
        time_zone: Option<chrono_tz::Tz>,
    ) -> Result<(i64, i64), QueueRunError> {
        let tx = JournalTransaction::begin(&mut self.connection)?;
        let existing = crate::workspace::find_run(tx.sql(), chat_id, run_key)?;
        let root = crate::workspace::ensure_root(tx.sql(), chat_id)?;
        if let Some((run, agent)) = existing {
            let saved = crate::journal::run_prompt(tx.sql(), run)?;
            // Migrated requests may belong to a previously independent root.
            let was_root: bool = tx.sql().query_row(
                "SELECT parent_agent_id IS NULL AND settings_json IS NOT NULL
                 FROM agent_sessions WHERE agent_id=?1",
                [agent],
                |row| row.get(0),
            )?;
            if !was_root || saved.as_deref() != Some(task) {
                return Err(QueueRunError::Conflict);
            }
            if let Some(requested) = time_zone
                && zone_for_chat(&tx, chat_id)? != requested
            {
                return Err(QueueRunError::Conflict);
            }
            tx.commit()?;
            return Ok((agent, run));
        }
        let busy: bool = tx.sql().query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_runs WHERE agent_id=?1 AND status IN ('runnable','waiting'))",
            [root], |row| row.get(0),
        )?;
        if busy {
            return Err(QueueRunError::Busy);
        }
        tx.sql().execute(
            "UPDATE agent_sessions SET time_zone=COALESCE(time_zone,?2),
                                      settings_json=COALESCE(settings_json,?3)
             WHERE agent_id=?1",
            params![
                root,
                time_zone.unwrap_or(chrono_tz::UTC).name(),
                serde_json::to_string(settings).map_err(JournalError::from)?
            ],
        )?;
        if time_zone.is_some() && time_zone != Some(zone_for_chat(&tx, chat_id)?) {
            return Err(QueueRunError::Conflict);
        }
        let run = crate::workspace::insert_run(tx.sql(), chat_id, root, run_key)?;
        admit(&tx, run, root, None, task, settings)?;
        tx.commit()?;
        Ok((root, run))
    }

    /// Settings saved for agents that can continue after restart.
    pub fn active_agent_settings(&self) -> Result<Vec<AgentSettings>, JournalError> {
        let mut stmt = self.connection.prepare(
            "SELECT DISTINCT s.settings_json FROM agent_sessions s WHERE s.settings_json IS NOT NULL
             AND (s.is_chat_root=1 OR EXISTS (SELECT 1 FROM agent_runs r WHERE r.agent_id=s.agent_id
                                               AND r.status IN ('runnable','waiting')))",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|s| Ok(serde_json::from_str(&s?)?))
            .collect()
    }

    /// List run identities, their parents, tasks and latest journal event in a chat.
    pub fn chat_runs(&self, chat: ChatId) -> Result<Vec<JsonValue>, JournalError> {
        let mut query = self.connection.prepare(
            "SELECT r.agent_id, s.parent_agent_id, r.run_key, r.status,
                    (SELECT json_extract(event_json, '$.data') FROM run_events
                     WHERE run_id=r.run_id ORDER BY sequence LIMIT 1),
                    (SELECT json_extract(event_json, '$.kind') FROM run_events
                     WHERE run_id=r.run_id ORDER BY sequence DESC LIMIT 1), r.run_id, s.spawned_by_run_id
             FROM agent_runs r JOIN agent_sessions s USING(agent_id)
             WHERE r.chat_id=?1 AND r.status<>'reserved' ORDER BY r.run_id",
        )?;
        let rows = query.query_map([chat], |row| {
            Ok(json!({
                "agent_id": row.get::<_, i64>(0)?,
                "run_id": row.get::<_, i64>(6)?,
                "parent_agent_id": row.get::<_, Option<i64>>(1)?,
                "spawned_by_run_id": row.get::<_, Option<i64>>(7)?,
                "run_key": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "task": row.get::<_, String>(4)?,
                "latest_event": row.get::<_, String>(5)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn zone_for_chat(tx: &JournalTransaction<'_>, chat: ChatId) -> Result<chrono_tz::Tz, JournalError> {
    let zone: String = tx.sql().query_row(
        "SELECT COALESCE(time_zone,'UTC') FROM agent_sessions WHERE chat_id=?1 AND is_chat_root=1",
        [chat],
        |r| r.get(0),
    )?;
    zone.parse()
        .map_err(|_| JournalError::InvalidWait("invalid saved chat time zone"))
}

fn admit(
    tx: &JournalTransaction<'_>,
    run: i64,
    agent: i64,
    parent: Option<i64>,
    task: &str,
    settings: &AgentSettings,
) -> Result<(), JournalError> {
    let tail = crate::journal::tail(tx.sql(), agent)?.map_or(0, |(sequence, _)| sequence);
    if tail != 0 {
        tx.append(run, tail, &[RunEvent::RunStarted(task.to_owned())])?;
        return Ok(());
    }
    let mut instructions = settings.instructions.clone();
    instructions.push_str("\nAvailable skills:");
    for (name, description) in &settings.catalog {
        if parent.is_none() || name != "agents" {
            instructions.push_str(&format!("\n{name}: {description}"));
        }
    }
    let mut run_details = format!("Your agent ID is {agent}.");
    if let Some(parent) = parent {
        run_details.push_str(&format!(" Your parent agent ID is {parent}."));
    }
    tx.append(
        run,
        0,
        &[
            RunEvent::RunStarted(task.to_owned()),
            RunEvent::StartupContext {
                instructions,
                run_details,
            },
        ],
    )?;
    Ok(())
}

pub(crate) fn spawn(
    tx: &JournalTransaction<'_>,
    parent: i64,
    key: &str,
    task: &str,
) -> Result<JsonValue, JournalError> {
    let (chat, parent_run, saved): (ChatId, i64, String) = tx
        .sql()
        .query_row(
            "SELECT s.chat_id, r.run_id, s.settings_json FROM agent_sessions s
         JOIN agent_runs r USING(agent_id) WHERE s.agent_id=?1 AND s.settings_json IS NOT NULL
         AND s.parent_agent_id IS NULL AND r.status='runnable'",
            [parent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or(JournalError::InvalidWait("only a root agent can spawn"))?;
    let child =
        crate::workspace::create_session(tx.sql(), chat, false, Some((parent, parent_run)))?;
    let run = crate::workspace::insert_run(tx.sql(), chat, child, key)?;
    tx.sql().execute(
        "UPDATE agent_sessions SET settings_json=?2 WHERE agent_id=?1",
        params![child, saved],
    )?;
    admit(
        tx,
        run,
        child,
        Some(parent),
        task,
        &serde_json::from_str(&saved)?,
    )?;
    Ok(json!({"agent_id": child, "run_id": run}))
}

pub(crate) fn finish(
    tx: &JournalTransaction<'_>,
    child: i64,
    result: Result<&str, &str>,
) -> Result<(), JournalError> {
    let parent: Option<(i64, i64)> = tx.sql().query_row(
        "SELECT parent_agent_id, spawned_by_run_id FROM agent_sessions WHERE agent_id = ?1 AND parent_agent_id IS NOT NULL",
        [child], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    if let Some((parent, parent_run)) = parent {
        let payload = match result {
            Ok(answer) => json!({"kind":"completed","answer":answer}),
            Err(error) => json!({"kind":"failed","error":error}),
        };
        crate::mailbox::record_sent_message(
            tx,
            &format!("outcome-{child}"),
            child,
            parent,
            crate::mailbox::MessageKind::ChildOutcome(parent_run),
            &payload,
        )?;
    }
    Ok(())
}

/// Spawn a child in the root's chat and immediately return its agent ID.
#[derive(Debug, Clone, Copy)]
pub struct SpawnAgent;

#[async_trait]
impl Tool for SpawnAgent {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "spawn",
            "Start an independent subagent on a task and return its agent_id.",
            json!({
                "type": "object",
                "properties": {"task": {"type": "string", "minLength": 1}},
                "required": ["task"],
                "additionalProperties": false,
            }),
        )
        .with_recovery(ToolRecovery::RetryAfterCrash)
    }
    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new("spawning requires a journaled root"))
    }
    async fn call_with_output(
        &self,
        _: ToolContext,
        args: &JsonValue,
    ) -> Result<ToolOutput, ToolError> {
        let task = args
            .as_object()
            .filter(|o| o.len() == 1)
            .and_then(|o| o.get("task"))
            .and_then(JsonValue::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::new("expected a nonempty task"))?;
        Ok(ToolOutput::SpawnAgent(task.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_rolls_back_with_parent_result_and_checks_saved_parent_link() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::open(directory.path()).unwrap();
        let chat = ChatId::new_v4();
        let settings = serde_json::from_value(json!({
            "model":"test","max_model_turns":10,"token_budget":100
        }))
        .unwrap();
        store
            .queue_root(chat, "root", "delegate", &settings, None)
            .unwrap();
        let mut journal = store.open_journal(chat, "root").unwrap();
        let effect =
            |tx: &JournalTransaction<'_>, parent| spawn(tx, parent, "child-run", "research");
        // The child is inserted before the parent's sequence check in this transaction.
        assert!(matches!(
            journal.commit_tool_result(1, "spawn".into(), effect),
            Err(JournalError::SequenceMismatch { .. })
        ));
        assert_eq!(store.chat_runs(chat).unwrap().len(), 1);
        let mut journal = store.open_journal(chat, "root").unwrap();
        let (_, RunEvent::ToolCallCompleted(result)) = journal
            .commit_tool_result(2, "spawn".into(), effect)
            .unwrap()
        else {
            panic!("expected spawn result")
        };
        let child = result.value["agent_id"].as_i64().unwrap();
        let tx = JournalTransaction::begin(&mut store.connection).unwrap();
        assert!(matches!(
            spawn(&tx, child, "nested", "grandchild"),
            Err(JournalError::InvalidWait("only a root agent can spawn"))
        ));
        drop(tx);
        assert_eq!(store.chat_runs(chat).unwrap().len(), 2);
    }
}
