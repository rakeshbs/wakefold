//! Messages between agents in one chat.

use async_trait::async_trait;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::json;

use crate::journal::append_events;
use crate::{
    JournalEntry, JournalError, JsonValue, PendingWait, RunEvent, Tool, ToolContext, ToolError,
    ToolOutput, ToolRecovery, ToolSpec, WaitOn,
};

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS mailbox (
        arrival_order INTEGER PRIMARY KEY,
        message_id TEXT NOT NULL UNIQUE,
        from_agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id),
        to_agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id),
        payload_json TEXT NOT NULL,
        consumed_sequence INTEGER
    );
    CREATE INDEX IF NOT EXISTS unread_mailbox ON mailbox(to_agent_id, arrival_order)
    WHERE consumed_sequence IS NULL;";

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
            "Receive the oldest matching message. Pause the run if none is available.",
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
    tx: &Transaction<'_>,
    message_id: &str,
    from_agent_id: i64,
    to_agent_id: i64,
    payload: &JsonValue,
) -> Result<(), JournalError> {
    let inserted = tx.execute(
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
    tx: &Transaction<'_>,
    to_agent_id: i64,
) -> Result<Option<JournalEntry>, JournalError> {
    let pending = tx
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
        .query_row(
            "SELECT message_id, from_agent_id, payload_json FROM mailbox
         WHERE to_agent_id = ?1 AND consumed_sequence IS NULL
           AND (?2 IS NULL OR from_agent_id = ?2)
         ORDER BY arrival_order LIMIT 1",
            params![to_agent_id, from_agent_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
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
    let sequence = append_events(tx, to_agent_id, wait_sequence, std::slice::from_ref(&event))?;
    tx.execute(
        "UPDATE mailbox SET consumed_sequence = ?1 WHERE message_id = ?2",
        params![sequence, message_id],
    )?;
    Ok(Some(JournalEntry { sequence, event }))
}
