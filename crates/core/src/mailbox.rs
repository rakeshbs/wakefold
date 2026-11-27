//! Messages from agents and host clients.

use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

use crate::journal::JournalTransaction;
use crate::{
    JournalEntry, JournalError, JsonValue, PendingWait, RunEvent, Tool, ToolContext, ToolError,
    ToolOutput, ToolRecovery, ToolSpec, WaitOn, WorkspaceStore,
};

/// A failure to post a client message to a run's mailbox.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientMessageError {
    /// The message key is empty.
    #[error("message_key must be nonempty")]
    EmptyKey,
    /// The recipient has no run journal.
    #[error("unknown run")]
    UnknownRun,
    /// The recipient has already finished or failed.
    #[error("the run has already ended")]
    RunEnded,
    /// This key already identifies a different message for the recipient.
    #[error("message_key already identifies a different payload")]
    KeyConflict,
    /// The mailbox or journal could not be read or advanced.
    #[error(transparent)]
    Journal(#[from] JournalError),
}

impl From<rusqlite::Error> for ClientMessageError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Journal(error.into())
    }
}

impl WorkspaceStore {
    /// Queue a client message for an existing run and return its message ID.
    ///
    /// Reusing the same key and payload for this run returns the original ID,
    /// including after the run ends. A matching mailbox wait wakes in the
    /// same transaction. A receive with a specific agent filter skips client messages.
    ///
    /// # Errors
    ///
    /// A new message requires an existing run that has not ended. Empty keys
    /// and attempts to reuse a key with a different payload are rejected.
    pub fn post_client_message(
        &mut self,
        agent_id: i64,
        message_key: &str,
        payload: &JsonValue,
    ) -> Result<String, ClientMessageError> {
        let run: Option<i64> = self
            .connection
            .query_row(
                "SELECT run_id FROM agent_runs WHERE agent_id=?1 ORDER BY run_id DESC LIMIT 1",
                [agent_id],
                |row| row.get(0),
            )
            .optional()?;
        self.post_run_message(
            run.ok_or(ClientMessageError::UnknownRun)?,
            message_key,
            payload,
        )
    }

    /// Send a host message to a particular run, including retries after it finishes.
    pub fn post_run_message(
        &mut self,
        run_id: i64,
        message_key: &str,
        payload: &JsonValue,
    ) -> Result<String, ClientMessageError> {
        if message_key.trim().is_empty() {
            return Err(ClientMessageError::EmptyKey);
        }
        let tx = JournalTransaction::begin(&mut self.connection)?;
        let previous = tx
            .sql()
            .query_row(
                "SELECT message_id, payload_json FROM mailbox_messages
             WHERE to_run_id = ?1 AND client_message_key = ?2",
                params![run_id, message_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((id, json)) = previous {
            let saved: JsonValue = serde_json::from_str(&json).map_err(JournalError::from)?;
            return if &saved == payload {
                Ok(id)
            } else {
                Err(ClientMessageError::KeyConflict)
            };
        }
        let saved: Option<(i64, String)> = tx
            .sql()
            .query_row(
                "SELECT agent_id,status FROM agent_runs WHERE run_id=?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((agent_id, status)) = saved else {
            return Err(ClientMessageError::UnknownRun);
        };
        match status.as_str() {
            "runnable" | "waiting" => {}
            "reserved" => return Err(ClientMessageError::UnknownRun),
            _ => return Err(ClientMessageError::RunEnded),
        }
        let id = format!("client-{:032x}", rand::random::<u128>());
        tx.sql().execute(
            "INSERT INTO mailbox_messages(message_id, chat_id, to_agent_id, message_kind, payload_json, client_message_key, to_run_id)
             SELECT ?1, chat_id, agent_id, 'message', ?3, ?4, run_id FROM agent_runs WHERE run_id=?2",
            params![
                id,
                run_id,
                serde_json::to_string(payload).map_err(JournalError::from)?,
                message_key
            ],
        )?;
        deliver_waiting_message(&tx, agent_id)?;
        tx.commit()?;
        Ok(id)
    }
}

/// Queue a JSON message for another agent in the current chat.
///
/// Add it to a skill such as `mailbox` to expose `mailbox.send`.
/// The host can supply recipient IDs from [`crate::RunWorkspace::agent_id`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SendMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArguments {
    to_agent_id: i64,
    payload: JsonValue,
}

#[async_trait]
impl Tool for SendMessage {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "send",
            "Queue a message for an agent in this chat.",
            json!({
                "type": "object",
                "properties": { "to_agent_id": { "type": "integer", "minimum": 1 }, "payload": {} },
                "required": ["to_agent_id", "payload"],
                "additionalProperties": false
            }),
        )
        .with_recovery(ToolRecovery::RetryAfterCrash)
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new("sending a message requires a journaled run"))
    }

    async fn call_with_output(
        &self,
        _: ToolContext,
        arguments: &JsonValue,
    ) -> Result<ToolOutput, ToolError> {
        let SendArguments {
            to_agent_id,
            payload,
        } = SendArguments::deserialize(arguments)
            .map_err(|error| ToolError::with_source("invalid mailbox send arguments", error))?;
        if to_agent_id <= 0 {
            return Err(ToolError::new("to_agent_id must be a positive integer"));
        }
        Ok(ToolOutput::SendMessage {
            to_agent_id,
            payload,
        })
    }
}

/// Receive a message from the run's mailbox, suspending until one arrives if needed.
///
/// Add it to a skill such as `mailbox` to expose `mailbox.receive`.
#[derive(Debug, Default, Clone, Copy)]
pub struct WaitForMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiveArguments {
    wait: Option<bool>,
    #[serde(default)]
    completion_only: bool,
    from_agent_id: Option<i64>,
}

#[async_trait]
impl Tool for WaitForMessage {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "receive",
            "Receive the oldest matching message. Omit from_agent_id to include client messages. Set wait=false to return null if none is available. Set completion_only=true for a child completion or failure.",
            json!({
                "type": "object",
                "properties": { "wait":{"type":"boolean"}, "completion_only":{"type":"boolean"}, "from_agent_id": { "type": ["integer", "null"], "minimum": 1 } },
                "additionalProperties": false
            }),
        )
        .with_recovery(ToolRecovery::RetryAfterCrash)
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new(
            "receiving a message requires a journaled run",
        ))
    }

    async fn call_with_output(
        &self,
        _: ToolContext,
        arguments: &JsonValue,
    ) -> Result<ToolOutput, ToolError> {
        let ReceiveArguments {
            from_agent_id,
            wait,
            completion_only,
        } = ReceiveArguments::deserialize(arguments)
            .map_err(|error| ToolError::with_source("invalid mailbox receive arguments", error))?;
        if from_agent_id.is_some_and(|id| id <= 0) {
            return Err(ToolError::new("from_agent_id must be a positive integer"));
        }
        if wait == Some(false) {
            return Ok(ToolOutput::CheckMailbox {
                from_agent_id,
                completion_only,
            });
        }
        Ok(ToolOutput::Wait(if completion_only {
            WaitOn::ChildOutcome { from_agent_id }
        } else {
            WaitOn::MailboxMessage { from_agent_id }
        }))
    }
}

pub(crate) enum MessageKind {
    Message,
    ChildOutcome(i64),
}

pub(crate) fn record_sent_message(
    tx: &JournalTransaction<'_>,
    message_id: &str,
    from_agent_id: i64,
    to_agent_id: i64,
    kind: MessageKind,
    payload: &JsonValue,
) -> Result<(), JournalError> {
    let inserted = tx.sql().execute(
        "INSERT INTO mailbox_messages(message_id, chat_id, from_agent_id, to_agent_id, payload_json, message_kind, to_run_id)
         SELECT ?1, sender.chat_id, sender.agent_id, recipient.agent_id, ?4, ?5,
                COALESCE(?6, CASE WHEN sender.parent_agent_id=recipient.agent_id THEN sender.spawned_by_run_id END)
         FROM agent_sessions sender JOIN agent_sessions recipient USING (chat_id)
         WHERE sender.agent_id = ?2 AND recipient.agent_id = ?3",
        params![
            message_id,
            from_agent_id,
            to_agent_id,
            serde_json::to_string(payload)?,
            match kind { MessageKind::Message => "message", MessageKind::ChildOutcome(_) => "child_outcome" },
            match kind { MessageKind::Message => None, MessageKind::ChildOutcome(run) => Some(run) }
        ],
    )?;
    if inserted == 0 {
        return Err(JournalError::InvalidMailboxRecipient {
            agent_id: to_agent_id,
        });
    }
    deliver_waiting_message(tx, to_agent_id)?;
    Ok(())
}

pub(crate) fn deliver_waiting_message(
    tx: &JournalTransaction<'_>,
    to_agent_id: i64,
) -> Result<Option<JournalEntry>, JournalError> {
    let pending = tx
        .sql()
        .query_row(
            "SELECT rw.wait_event_sequence, e.event_json, rw.run_id
         FROM agent_runs rw
         JOIN run_events e ON e.run_id = rw.run_id AND e.sequence = rw.wait_event_sequence
         WHERE rw.agent_id = ?1 AND rw.wait_kind = 'mailbox_message'",
            [to_agent_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((wait_sequence, wait_json, run_id)) = pending else {
        return Ok(None);
    };
    let RunEvent::WaitStarted(PendingWait { id, on }) = serde_json::from_str(&wait_json)? else {
        return Err(JournalError::InvalidWait("expected a saved mailbox wait"));
    };
    let (sender, only) = match on {
        WaitOn::MailboxMessage { from_agent_id } => (from_agent_id, false),
        WaitOn::ChildOutcome { from_agent_id } => (from_agent_id, true),
        _ => return Err(JournalError::InvalidWait("expected mailbox wait")),
    };
    let Some(payload) = take_message(tx, run_id, sender, only, wait_sequence + 1)? else {
        return Ok(None);
    };
    let event = RunEvent::Woken {
        wait_id: id,
        payload,
    };
    let sequence = tx.append(run_id, wait_sequence, std::slice::from_ref(&event))?;
    Ok(Some(JournalEntry {
        sequence,
        event,
        run_id,
    }))
}

pub(crate) fn take_message(
    tx: &JournalTransaction<'_>,
    run_id: i64,
    from_agent_id: Option<i64>,
    completion_only: bool,
    sequence: u64,
) -> Result<Option<JsonValue>, JournalError> {
    let message = tx
        .sql()
        .query_row(
            "SELECT message_id, from_agent_id, payload_json FROM mailbox_messages
         WHERE to_agent_id = (SELECT agent_id FROM agent_runs WHERE run_id=?1)
           AND (to_run_id IS NULL OR to_run_id=?1) AND received_event_sequence IS NULL
           AND (?2 IS NULL OR from_agent_id = ?2) AND (NOT ?3 OR message_kind = 'child_outcome')
         ORDER BY arrival_order LIMIT 1",
            params![run_id, from_agent_id, completion_only],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((message_id, sender, payload)) = message else {
        return Ok(None);
    };
    let payload: JsonValue = serde_json::from_str(&payload)?;
    tx.sql().execute(
        "UPDATE mailbox_messages SET received_event_sequence = ?1 WHERE message_id = ?2",
        params![sequence, message_id],
    )?;
    Ok(Some(
        json!({"message_id":message_id,"from_agent_id":sender,"payload":payload}),
    ))
}

impl WorkspaceStore {
    /// Read a page of mailbox history without consuming messages.
    pub fn mailbox_messages(&self, agent: i64, after: i64) -> Result<Vec<JsonValue>, JournalError> {
        self.read_mailbox(agent, None, after)
    }

    /// Read messages addressed to this run, along with messages shared by the agent.
    pub fn run_mailbox(&self, run_id: i64, after: i64) -> Result<Vec<JsonValue>, JournalError> {
        let agent: Option<i64> = self
            .connection
            .query_row(
                "SELECT agent_id FROM agent_runs WHERE run_id=?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        match agent {
            Some(agent) => self.read_mailbox(agent, Some(run_id), after),
            None => Ok(Vec::new()),
        }
    }

    fn read_mailbox(
        &self,
        agent: i64,
        run: Option<i64>,
        after: i64,
    ) -> Result<Vec<JsonValue>, JournalError> {
        let mut query = self.connection.prepare(
            "SELECT arrival_order, message_id, from_agent_id, payload_json, received_event_sequence, message_kind, to_run_id
             FROM mailbox_messages WHERE to_agent_id = ?1 AND arrival_order > ?2
               AND (?3 IS NULL OR to_run_id IS NULL OR to_run_id=?3)
             ORDER BY arrival_order LIMIT 128",
        )?;
        let mut rows = query.query(params![agent, after, run])?;
        let mut messages = Vec::new();
        while let Some(row) = rows.next()? {
            messages.push(json!({
                "arrival_order": row.get::<_, i64>(0)?,
                "message_id": row.get::<_, String>(1)?,
                "from_agent_id": row.get::<_, Option<i64>>(2)?,
                "payload": serde_json::from_str::<JsonValue>(&row.get::<_, String>(3)?)?,
                "message_kind": row.get::<_, String>(5)?,
                "to_run_id": row.get::<_, Option<i64>>(6)?,
                "consumed_sequence": row.get::<_, Option<i64>>(4)?,
                "received_event_sequence": row.get::<_, Option<i64>>(4)?,
            }));
        }
        Ok(messages)
    }
}
