//! Messages from agents and host clients.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

use crate::journal::{JournalTransaction, tail};
use crate::{
    JournalEntry, JournalError, JsonValue, PendingWait, RunEvent, Tool, ToolContext, ToolError,
    ToolOutput, ToolRecovery, ToolSpec, WaitOn, WorkspaceStore,
};

const SCHEMA: &str = "
    CREATE TABLE mailbox (
        arrival_order INTEGER PRIMARY KEY,
        message_id TEXT NOT NULL UNIQUE,
        from_agent_id INTEGER REFERENCES workspace_runs(agent_id),
        to_agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id),
        payload_json TEXT NOT NULL,
        consumed_sequence INTEGER,
        client_message_key TEXT,
        CHECK ((from_agent_id IS NULL) = (client_message_key IS NOT NULL))
    );
    CREATE INDEX unread_mailbox ON mailbox(to_agent_id, arrival_order)
    WHERE consumed_sequence IS NULL;
    CREATE UNIQUE INDEX client_message_keys ON mailbox(to_agent_id, client_message_key)
    WHERE client_message_key IS NOT NULL;";

pub(crate) fn initialize_schema(connection: &mut Connection) -> Result<(), rusqlite::Error> {
    if has_client_messages(connection)? {
        return Ok(());
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another connection may have upgraded the schema while this one waited.
    if has_client_messages(&tx)? {
        return Ok(());
    }
    let mailbox_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'mailbox')",
        [],
        |row| row.get(0),
    )?;
    if mailbox_exists {
        tx.execute_batch(
            "ALTER TABLE mailbox RENAME TO mailbox_before_client_messages;
             DROP INDEX IF EXISTS unread_mailbox;",
        )?;
    }
    tx.execute_batch(SCHEMA)?;
    if mailbox_exists {
        // Arrival order and consumption are part of the mailbox state.
        tx.execute_batch(
            "INSERT INTO mailbox(arrival_order, message_id, from_agent_id, to_agent_id,
                                 payload_json, consumed_sequence)
             SELECT arrival_order, message_id, from_agent_id, to_agent_id, payload_json,
                    consumed_sequence FROM mailbox_before_client_messages;
             DROP TABLE mailbox_before_client_messages;",
        )?;
    }
    tx.commit()
}

fn has_client_messages(connection: &Connection) -> Result<bool, rusqlite::Error> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('mailbox')
                       WHERE name = 'client_message_key')",
        [],
        |row| row.get(0),
    )
}

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
        if message_key.trim().is_empty() {
            return Err(ClientMessageError::EmptyKey);
        }
        let tx = JournalTransaction::begin(&mut self.connection)?;
        let previous = tx
            .sql()
            .query_row(
                "SELECT message_id, payload_json FROM mailbox
             WHERE to_agent_id = ?1 AND client_message_key = ?2",
                params![agent_id, message_key],
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
        let Some((_, terminal)) = tail(tx.sql(), agent_id)? else {
            return Err(ClientMessageError::UnknownRun);
        };
        if terminal {
            return Err(ClientMessageError::RunEnded);
        }
        let id = format!("client-{:032x}", rand::random::<u128>());
        tx.sql().execute(
            "INSERT INTO mailbox(message_id, to_agent_id, payload_json, client_message_key)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                agent_id,
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
    from_agent_id: Option<i64>,
}

#[async_trait]
impl Tool for WaitForMessage {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "receive",
            "Receive the oldest matching message. Omit from_agent_id to include client messages. Pause the run if none is available.",
            json!({
                "type": "object",
                "properties": { "from_agent_id": { "type": ["integer", "null"], "minimum": 1 } },
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
        let ReceiveArguments { from_agent_id } = ReceiveArguments::deserialize(arguments)
            .map_err(|error| ToolError::with_source("invalid mailbox receive arguments", error))?;
        if from_agent_id.is_some_and(|id| id <= 0) {
            return Err(ToolError::new("from_agent_id must be a positive integer"));
        }
        Ok(ToolOutput::Wait(WaitOn::MailboxMessage { from_agent_id }))
    }
}

pub(crate) fn record_sent_message(
    tx: &JournalTransaction<'_>,
    message_id: &str,
    from_agent_id: i64,
    to_agent_id: i64,
    payload: &JsonValue,
) -> Result<(), JournalError> {
    let inserted = tx.sql().execute(
        "INSERT INTO mailbox(message_id, from_agent_id, to_agent_id, payload_json)
         SELECT ?1, sender.agent_id, recipient.agent_id, ?4
         FROM workspace_runs sender JOIN workspace_runs recipient USING (workspace_id)
         WHERE sender.agent_id = ?2 AND recipient.agent_id = ?3",
        params![
            message_id,
            from_agent_id,
            to_agent_id,
            serde_json::to_string(payload)?
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
            "SELECT rw.started_sequence, e.event_json
         FROM run_waits rw
         JOIN run_events e ON e.agent_id = rw.agent_id AND e.sequence = rw.started_sequence
         WHERE rw.agent_id = ?1 AND rw.source = 'mailbox_message' AND rw.resolved = 0",
            [to_agent_id],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((wait_sequence, wait_json)) = pending else {
        return Ok(None);
    };
    let RunEvent::WaitStarted(PendingWait {
        id,
        on: WaitOn::MailboxMessage { from_agent_id },
    }) = serde_json::from_str(&wait_json)?
    else {
        return Err(JournalError::InvalidWait("expected a saved mailbox wait"));
    };
    let message = tx
        .sql()
        .query_row(
            "SELECT message_id, from_agent_id, payload_json FROM mailbox
         WHERE to_agent_id = ?1 AND consumed_sequence IS NULL
           AND (?2 IS NULL OR from_agent_id = ?2)
         ORDER BY arrival_order LIMIT 1",
            params![to_agent_id, from_agent_id],
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
    let event = RunEvent::Woken {
        wait_id: id,
        payload: json!({ "message_id": message_id, "from_agent_id": sender, "payload": payload }),
    };
    let sequence = tx.append(to_agent_id, wait_sequence, std::slice::from_ref(&event))?;
    tx.sql().execute(
        "UPDATE mailbox SET consumed_sequence = ?1 WHERE message_id = ?2",
        params![sequence, message_id],
    )?;
    Ok(Some(JournalEntry { sequence, event }))
}
