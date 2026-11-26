//! Durable admission of top-level runs to the scheduler.

use rusqlite::{OptionalExtension, Transaction, params};
use thiserror::Error;

use crate::journal::JournalTransaction;
use crate::workspace::reserve_run;
use crate::{JournalError, RunEvent, WorkspaceStore};

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS queued_runs (
        agent_id INTEGER PRIMARY KEY REFERENCES workspace_runs(agent_id),
        agent_name TEXT NOT NULL,
        ready INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX IF NOT EXISTS ready_queued_runs ON queued_runs(agent_id) WHERE ready = 1;";

/// A failure to admit a run to the scheduler.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum QueueRunError {
    /// The chat and run keys belong to a different request.
    #[error("these chat and run keys already belong to a different request")]
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

impl WorkspaceStore {
    /// Queue a run for the named agent and return its stable ID.
    ///
    /// The scheduler receives the saved agent name and prompt, including after
    /// a restart. Repeating the same request with the same keys returns its ID.
    ///
    /// # Errors
    ///
    /// Returns [`QueueRunError::Conflict`] if the keys already identify a different run.
    pub fn queue_run(
        &mut self,
        chat_key: &str,
        run_key: &str,
        agent_name: &str,
        prompt: &str,
    ) -> Result<i64, QueueRunError> {
        let tx = JournalTransaction::begin(&mut self.connection)?;
        let (_, agent_id) = reserve_run(tx.sql(), chat_key, run_key)?;
        let saved = tx
            .sql()
            .query_row(
                "SELECT event_json, agent_name FROM run_events
             LEFT JOIN queued_runs USING (agent_id)
             WHERE agent_id = ?1 AND sequence = 1",
                [agent_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        if let Some((event, name)) = saved {
            let previous: RunEvent = serde_json::from_str(&event).map_err(JournalError::from)?;
            if name.as_deref() != Some(agent_name)
                || !matches!(previous, RunEvent::RunStarted(text) if text == prompt)
            {
                return Err(QueueRunError::Conflict);
            }
        } else {
            tx.sql().execute(
                "INSERT INTO queued_runs(agent_id, agent_name) VALUES (?1, ?2)",
                params![agent_id, agent_name],
            )?;
            tx.append(agent_id, 0, &[RunEvent::RunStarted(prompt.into())])?;
        }
        tx.commit()?;
        Ok(agent_id)
    }
}

pub(crate) fn clear_ready(tx: &Transaction<'_>, agent_id: i64) -> Result<(), JournalError> {
    tx.execute(
        "UPDATE queued_runs SET ready = 0 WHERE agent_id = ?1 AND ready = 1",
        [agent_id],
    )?;
    Ok(())
}
