//! SQLite projection of outstanding waits.

use std::num::NonZeroUsize;
use std::time::SystemTime;

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::{WaitOn, WakeEvent, current_millis, deadline_millis};
use crate::journal::JournalTransaction;
use crate::{JournalError, JsonValue, PendingWait, RunEvent, WorkspaceStore};

pub(crate) fn record_transition(
    tx: &JournalTransaction<'_>,
    agent_id: i64,
    sequence: u64,
    event: &RunEvent,
) -> Result<(), JournalError> {
    let sql = tx.sql();
    let status = match event {
        RunEvent::RunStarted(_) => "runnable",
        RunEvent::MailboxMessageSent {
            message_id,
            to_agent_id,
            payload,
        } => {
            crate::mailbox::record_sent_message(
                tx,
                message_id,
                agent_id,
                *to_agent_id,
                crate::mailbox::MessageKind::Message,
                payload,
            )?;
            return Ok(());
        }
        RunEvent::WaitStarted(wait) => {
            let wake_at = match wait.on {
                WaitOn::Timer { wake_at } => Some(
                    deadline_millis(wake_at)
                        .ok_or(JournalError::InvalidWait("timer deadline is out of range"))?,
                ),
                _ => None,
            };
            let inserted = sql.execute(
                "INSERT INTO run_waits(agent_id, wait_id, wait_kind, wake_at_ms, start_event_sequence)
                 VALUES(?1, ?2, ?3, ?4, ?5) ON CONFLICT(agent_id) DO NOTHING",
                params![agent_id, wait.id, wait.on.source(), wake_at, sequence],
            )?;
            if inserted == 0 {
                return Err(JournalError::InvalidWait("run already has a pending wait"));
            }
            "waiting"
        }
        RunEvent::Woken { wait_id, .. } => {
            let updated = sql.execute(
                "DELETE FROM run_waits
                 WHERE agent_id = ?1 AND wait_id = ?2",
                params![agent_id, wait_id],
            )?;
            if updated == 0 {
                return Err(JournalError::InvalidWait("no matching pending wait"));
            }
            "runnable"
        }
        RunEvent::RunFinished(answer) => {
            sql.execute("DELETE FROM run_waits WHERE agent_id = ?1", [agent_id])?;
            crate::dynamic::finish(tx, agent_id, Ok(answer))?;
            "completed"
        }
        RunEvent::RunFailed(error) => {
            sql.execute("DELETE FROM run_waits WHERE agent_id = ?1", [agent_id])?;
            crate::dynamic::finish(tx, agent_id, Err(error))?;
            "failed"
        }
        _ => return Ok(()),
    };
    sql.execute(
        "UPDATE agent_runs SET status=?2 WHERE agent_id=?1",
        params![agent_id, status],
    )?;
    Ok(())
}

impl WorkspaceStore {
    /// Deliver an event to a pending wait with the matching identifier and source.
    ///
    /// Returns whether the event was recorded. Repeated, unmatched, or late
    /// deliveries return false. A recorded event makes the run ready to resume.
    ///
    /// # Errors
    ///
    /// Returns a journal error if the transaction cannot be committed.
    pub fn resolve_wait(&mut self, wait_id: &str, event: WakeEvent) -> Result<bool, JournalError> {
        let approval = match &event {
            WakeEvent::Approval { approved } => Some(*approved),
            _ => None,
        };
        let (source, payload) = event.into_result();
        let tx = JournalTransaction::begin(&mut self.connection)?;
        let pending = tx
            .sql()
            .query_row(
                "SELECT agent_id, start_event_sequence FROM run_waits
                 WHERE wait_id = ?1
                   AND (wait_kind = ?2 OR (?2 = 'approval' AND wait_kind = 'tool_approval'))",
                params![wait_id, source],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((agent_id, sequence)) = pending {
            record_wake(&tx, agent_id, sequence, wait_id.into(), payload)?;
        }
        tx.commit()?;
        if let (Some((agent_id, _)), Some(approved)) = (pending, approval) {
            tracing::info!(target: "wakefold::journal", agent_id, wait_id, approved, "approval decision committed");
        }
        Ok(pending.is_some())
    }

    /// Find recorded tool approval requests, including requests saved before a restart.
    ///
    /// Each pair contains the requesting agent's stable ID and the wait to pass
    /// to [`Self::resolve_wait`] by ID when a person makes a decision.
    ///
    /// # Errors
    ///
    /// Returns a journal error if an approval cannot be read or decoded.
    pub fn pending_tool_approvals(&self) -> Result<Vec<(i64, PendingWait)>, JournalError> {
        let mut query = self.connection.prepare_cached(
            "SELECT rw.agent_id, e.event_json FROM run_waits rw
             JOIN run_events e ON e.agent_id = rw.agent_id AND e.sequence = rw.start_event_sequence
             WHERE rw.wait_kind = 'tool_approval'
             ORDER BY rw.agent_id",
        )?;
        let mut rows = query.query([])?;
        let mut approvals = Vec::new();
        while let Some(row) = rows.next()? {
            let value: String = row.get(1)?;
            match serde_json::from_str(&value)? {
                RunEvent::WaitStarted(
                    wait @ PendingWait {
                        on: WaitOn::ToolApproval { .. },
                        ..
                    },
                ) => {
                    approvals.push((row.get(0)?, wait));
                }
                _ => return Err(JournalError::InvalidWait("expected a saved tool approval")),
            }
        }
        Ok(approvals)
    }

    /// Resolve up to `limit` timers due at or before `now`.
    ///
    /// The returned count includes timers resolved in this transaction.
    ///
    /// # Errors
    ///
    /// Returns a journal error if the transaction cannot be committed.
    pub fn wake_due_timers(
        &mut self,
        now: SystemTime,
        limit: NonZeroUsize,
    ) -> Result<usize, JournalError> {
        let tx = JournalTransaction::begin(&mut self.connection)?;
        let due = {
            let mut query = tx.sql().prepare_cached(
                "SELECT agent_id, start_event_sequence, wait_id, wake_at_ms FROM run_waits
                WHERE wake_at_ms IS NOT NULL AND wake_at_ms <= ?1
                ORDER BY wake_at_ms, agent_id LIMIT ?2",
            )?;
            let now = current_millis(now);
            let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
            query
                .query_map(params![now, limit], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, i64>(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let count = due.len();
        for (agent_id, sequence, wait_id, wake_at_ms) in due {
            let payload = json!({ "wake_at_unix_ms": wake_at_ms });
            record_wake(&tx, agent_id, sequence, wait_id, payload)?;
        }
        tx.commit()?;
        Ok(count)
    }
}

fn record_wake(
    tx: &JournalTransaction<'_>,
    agent_id: i64,
    sequence: u64,
    wait_id: String,
    payload: JsonValue,
) -> Result<(), JournalError> {
    tx.append(agent_id, sequence, &[RunEvent::Woken { wait_id, payload }])?;
    Ok(())
}
