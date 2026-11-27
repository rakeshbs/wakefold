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
    /// Admit a root using a snapshot of its defaults. Retries return the original agent ID.
    pub fn queue_root(
        &mut self,
        chat_id: ChatId,
        run_key: &str,
        task: &str,
        settings: &AgentSettings,
        time_zone: Option<chrono_tz::Tz>,
    ) -> Result<i64, QueueRunError> {
        let tx = JournalTransaction::begin(&mut self.connection)?;
        let id = crate::workspace::reserve_run(tx.sql(), chat_id, run_key)?;
        let previous: Option<(String, bool)> = tx
            .sql()
            .query_row(
                "SELECT event_json, parent_agent_id IS NULL AND settings_json IS NOT NULL
             FROM agent_runs JOIN run_events USING(agent_id) WHERE agent_id = ?1 AND sequence = 1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        tx.sql().execute(
            "UPDATE chats SET time_zone = ?2 WHERE chat_id = ?1 AND time_zone IS NULL",
            params![chat_id, time_zone.unwrap_or(chrono_tz::UTC).name()],
        )?;
        let zone = zone_for_run(&tx, id)?;
        if time_zone.is_some_and(|requested| requested != zone) {
            return Err(QueueRunError::Conflict);
        }
        if let Some((event, root)) = previous {
            if !root
                || !matches!(serde_json::from_str(&event).map_err(JournalError::from)?, RunEvent::RunStarted(saved) if saved==task)
            {
                return Err(QueueRunError::Conflict);
            }
        } else {
            admit(&tx, id, None, task, settings, zone)?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Settings for dynamic runs that may still need a provider after restart.
    pub fn active_agent_settings(&self) -> Result<Vec<AgentSettings>, JournalError> {
        let mut stmt = self.connection.prepare(
            "SELECT DISTINCT settings_json FROM agent_runs WHERE settings_json IS NOT NULL
             AND status IN ('runnable','waiting')",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|s| Ok(serde_json::from_str(&s?)?))
            .collect()
    }

    /// List run identities, their parents, tasks and latest journal event in a chat.
    pub fn chat_runs(&self, chat: ChatId) -> Result<Vec<JsonValue>, JournalError> {
        let mut query = self.connection.prepare(
            "SELECT wr.agent_id, wr.parent_agent_id, wr.run_key, wr.status,
                    json_extract(first.event_json, '$.data'),
                    (SELECT json_extract(event_json, '$.kind') FROM run_events e
                     WHERE e.agent_id = wr.agent_id ORDER BY sequence DESC LIMIT 1)
             FROM agent_runs wr
             JOIN run_events first ON first.agent_id = wr.agent_id AND first.sequence = 1
             WHERE chat_id = ?1 ORDER BY wr.agent_id",
        )?;
        let rows = query.query_map([chat], |row| {
            Ok(json!({
                "agent_id": row.get::<_, i64>(0)?,
                "parent_agent_id": row.get::<_, Option<i64>>(1)?,
                "run_key": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "task": row.get::<_, String>(4)?,
                "latest_event": row.get::<_, String>(5)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn zone_for_run(tx: &JournalTransaction<'_>, id: i64) -> Result<chrono_tz::Tz, JournalError> {
    let zone: String = tx.sql().query_row(
        "SELECT time_zone FROM chats JOIN agent_runs USING(chat_id) WHERE agent_id=?1",
        [id],
        |r| r.get(0),
    )?;
    zone.parse()
        .map_err(|_| JournalError::InvalidWait("invalid saved chat time zone"))
}

fn admit(
    tx: &JournalTransaction<'_>,
    id: i64,
    parent: Option<i64>,
    task: &str,
    settings: &AgentSettings,
    zone: chrono_tz::Tz,
) -> Result<(), JournalError> {
    tx.sql().execute(
        "UPDATE agent_runs SET parent_agent_id=?2, settings_json=?3 WHERE agent_id=?1",
        params![id, parent, serde_json::to_string(settings)?],
    )?;
    let mut instructions = settings.instructions.clone();
    instructions.push_str("\nAvailable skills:");
    for (name, description) in &settings.catalog {
        if parent.is_none() || name != "agents" {
            instructions.push_str(&format!("\n{name}: {description}"));
        }
    }
    let date = chrono::Utc::now().with_timezone(&zone).date_naive();
    let mut run_details = format!("Start date: {date} ({zone}). Your agent ID is {id}.");
    if let Some(parent) = parent {
        run_details.push_str(&format!(" Your parent agent ID is {parent}."));
    }
    tx.append(
        id,
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
    let saved: String = tx
        .sql()
        .query_row(
            "SELECT settings_json FROM agent_runs WHERE agent_id=?1 AND settings_json IS NOT NULL AND parent_agent_id IS NULL",
            [parent],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(JournalError::InvalidWait("only a root agent can spawn"))?;
    let child = tx.sql().query_row(
        "INSERT INTO agent_runs(chat_id, run_key)
         SELECT chat_id, ?2 FROM agent_runs WHERE agent_id = ?1 RETURNING agent_id",
        params![parent, key],
        |row| row.get(0),
    )?;
    admit(
        tx,
        child,
        Some(parent),
        task,
        &serde_json::from_str(&saved)?,
        zone_for_run(tx, parent)?,
    )?;
    Ok(json!({"agent_id":child}))
}

pub(crate) fn finish(
    tx: &JournalTransaction<'_>,
    child: i64,
    result: Result<&str, &str>,
) -> Result<(), JournalError> {
    let parent: Option<i64> = tx.sql().query_row(
        "SELECT parent_agent_id FROM agent_runs WHERE agent_id = ?1 AND parent_agent_id IS NOT NULL",
        [child], |row| row.get(0),
    ).optional()?;
    if let Some(parent) = parent {
        let payload = match result {
            Ok(answer) => json!({"kind":"completed","answer":answer}),
            Err(error) => json!({"kind":"failed","error":error}),
        };
        crate::mailbox::record_sent_message(
            tx,
            &format!("outcome-{child}"),
            child,
            parent,
            crate::mailbox::MessageKind::ChildOutcome,
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
