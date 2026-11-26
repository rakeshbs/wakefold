//! SQLite projection of pending and resolved waits.

use std::num::NonZeroUsize;
use std::time::SystemTime;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;

use super::{WaitOn, WakeEvent, current_millis, deadline_millis};
use crate::journal::append_events;
use crate::{JournalError, JsonValue, RunEvent, WorkspaceStore};

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS run_waits (
        agent_id INTEGER PRIMARY KEY REFERENCES workspace_runs(agent_id),
        wait_id TEXT NOT NULL UNIQUE,
        source TEXT NOT NULL,
        wake_at_ms INTEGER,
        started_sequence INTEGER NOT NULL,
        resolved INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS pending_timers ON run_waits(wake_at_ms, agent_id)
    WHERE wake_at_ms IS NOT NULL AND resolved = 0;
    CREATE INDEX IF NOT EXISTS ready_waits ON run_waits(agent_id) WHERE resolved = 1;";

pub(crate) fn record_transition(
    tx: &Transaction<'_>,
    agent_id: i64,
    sequence: u64,
    event: &RunEvent,
) -> Result<(), JournalError> {
    match event {
        RunEvent::WaitStarted(wait) => {
            let wake_at = match wait.on {
                WaitOn::Timer { wake_at } => Some(
                    deadline_millis(wake_at)
                        .ok_or(JournalError::InvalidWait("timer deadline is out of range"))?,
                ),
                _ => None,
            };
            let inserted = tx.execute(
                "INSERT INTO run_waits(agent_id, wait_id, source, wake_at_ms, started_sequence)
                VALUES(?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(agent_id) DO UPDATE SET wait_id = excluded.wait_id,
                    source = excluded.source, wake_at_ms = excluded.wake_at_ms,
                    started_sequence = excluded.started_sequence, resolved = 0
                WHERE run_waits.resolved = 1",
                params![agent_id, wait.id, wait.on.source(), wake_at, sequence],
            )?;
            if inserted == 0 {
                return Err(JournalError::InvalidWait("run already has a pending wait"));
            }
            crate::delegation::clear_start(tx, agent_id)?;
            if let WaitOn::Delegation { worker, prompt } = &wait.on {
                crate::delegation::start_worker(tx, agent_id, &wait.id, worker, prompt)?;
            }
        }
        RunEvent::Woken { wait_id, .. } => {
            let updated = tx.execute(
                "UPDATE run_waits SET resolved = 1
                 WHERE agent_id = ?1 AND wait_id = ?2 AND resolved = 0",
                params![agent_id, wait_id],
            )?;
            if updated == 0 {
                return Err(JournalError::InvalidWait("no matching pending wait"));
            }
        }
        RunEvent::RunFinished(answer) => {
            tx.execute("DELETE FROM run_waits WHERE agent_id = ?1", [agent_id])?;
            crate::delegation::finish_worker(tx, agent_id, answer)?;
        }
        _ => {}
    }
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
        let (source, payload) = event.into_result();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = tx
            .query_row(
                "SELECT agent_id, started_sequence FROM run_waits
                 WHERE wait_id = ?1 AND source = ?2 AND resolved = 0",
                params![wait_id, source],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((agent_id, sequence)) = pending {
            record_wake(&tx, agent_id, sequence, wait_id.into(), payload)?;
        }
        tx.commit()?;
        Ok(pending.is_some())
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
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let due = {
            let mut query = tx.prepare_cached(
                "SELECT agent_id, started_sequence, wait_id, wake_at_ms FROM run_waits
                WHERE resolved = 0 AND wake_at_ms IS NOT NULL AND wake_at_ms <= ?1
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
    tx: &Transaction<'_>,
    agent_id: i64,
    sequence: u64,
    wait_id: String,
    payload: JsonValue,
) -> Result<(), JournalError> {
    append_events(
        tx,
        agent_id,
        sequence,
        &[RunEvent::Woken { wait_id, payload }],
    )?;
    Ok(())
}
