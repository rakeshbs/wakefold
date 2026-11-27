//! Resume durable runs when their saved waits resolve.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::pin;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use futures_channel::oneshot;
use futures_timer::Delay;
use futures_util::future::{Either, select};
use futures_util::stream::{FuturesUnordered, StreamExt};
use thiserror::Error;

use crate::wait::current_millis;
use crate::{
    CancellationToken, ChatId, JournalError, Provider, RunError, RunOptions, RunOutcome, Skill,
    WorkspaceError, WorkspaceStore,
};

// Independent store handles and wall-clock adjustments can change the next wake time.
const MAX_IDLE_DELAY: Duration = Duration::from_millis(250);
const MIN_IDLE_DELAY: Duration = Duration::from_millis(1);

/// Identity of a durable run ready to start or resume.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScheduledRun {
    /// Identity of the particular request being scheduled.
    pub run_id: i64,
    /// ID assigned by the workspace store.
    pub agent_id: i64,
    /// Chat UUID used to open the journal.
    pub chat_id: ChatId,
    /// Run key used to open the journal.
    pub run_key: String,
    /// The root that spawned this child, if any.
    pub parent_agent_id: Option<i64>,
    /// Settings saved at admission. Direct SDK runs supply their configuration through the host.
    pub settings: Option<Box<crate::AgentSettings>>,
}

/// Configuration the host supplies when the scheduler resumes a run.
#[non_exhaustive]
pub struct ScheduledRunConfig<'a> {
    provider: &'a dyn Provider,
    skills: &'a [Skill<'a>],
    options: RunOptions,
}

impl<'a> ScheduledRunConfig<'a> {
    /// Use this provider and skill catalog to resume the scheduled run.
    pub fn new(provider: &'a dyn Provider, skills: &'a [Skill<'a>], options: RunOptions) -> Self {
        Self {
            provider,
            skills,
            options,
        }
    }
}

impl std::fmt::Debug for ScheduledRunConfig<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScheduledRunConfig")
            .field("skill_count", &self.skills.len())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// A failure to find or resume a run ready to wake.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SchedulerError {
    /// A database operation on the wait queue failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// The scheduler could not start its SQLite worker.
    #[error("could not start scheduler storage worker")]
    StartWorker(#[source] std::io::Error),
    /// The storage worker exited without returning a response.
    #[error("scheduler storage worker stopped")]
    WorkerStopped,
    /// A scheduled run's workspace could not be opened.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// The host has no configuration for a ready run.
    #[error("no agent configuration for run {} in chat {}", .0.run_key, .0.chat_id)]
    MissingAgent(ScheduledRun),
    /// A driver returned an error while resuming a scheduled run.
    #[error("could not resume run {} in chat {}", run.run_key, run.chat_id)]
    Resume {
        /// Run whose driver failed.
        run: ScheduledRun,
        /// Error from the driver.
        #[source]
        source: Box<RunError>,
    },
}

struct ReadyRun {
    run: ScheduledRun,
    prompt: String,
}

struct Schedule {
    ready: Vec<ReadyRun>,
    delay: Duration,
}

impl WorkspaceStore {
    /// Drive queued runs and resolved waits until cancelled or a run fails.
    ///
    /// Startup finds overdue timers and wakes already committed before a restart.
    /// Supply the original provider, skill catalog and limits for each run from
    /// `configure`. The scheduler reads the original prompt from its journal.
    /// `on_outcome` receives each finished or newly suspended run.
    ///
    /// At most `max_concurrent` runs are active at once. Cancelling the scheduler
    /// drops its active drivers, which can be recovered from the journal. A
    /// suspended run holds no driver and uses no slot. The scheduler checks
    /// independent database writers at least every 250 ms while idle.
    ///
    /// # Errors
    ///
    /// A storage or driver failure stops the scheduler. Ready runs stay in the
    /// database until they finish or suspend again.
    pub async fn run_scheduler<'a>(
        self,
        max_concurrent: NonZeroUsize,
        cancellation: &CancellationToken,
        mut configure: impl FnMut(&ScheduledRun) -> ScheduledRunConfig<'a>,
        on_outcome: impl FnMut(&ScheduledRun, &RunOutcome),
    ) -> Result<(), SchedulerError> {
        self.run_scheduler_with_agent_lookup(
            max_concurrent,
            cancellation,
            |run| Some(configure(run)),
            on_outcome,
        )
        .await
    }

    /// Drive runnable SDK and daemon runs, looking up each provider in the host.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::MissingAgent`] for an unknown run configuration.
    pub async fn run_scheduler_with_agent_lookup<'a>(
        self,
        max_concurrent: NonZeroUsize,
        cancellation: &CancellationToken,
        configure: impl FnMut(&ScheduledRun) -> Option<ScheduledRunConfig<'a>>,
        on_outcome: impl FnMut(&ScheduledRun, &RunOutcome),
    ) -> Result<(), SchedulerError> {
        self.drive_scheduler(false, max_concurrent, cancellation, configure, on_outcome)
            .await
    }

    /// Drive admitted agents using their saved settings and the host's provider lookup.
    /// Direct SDK runs use the host-supplied configuration in [`Self::run_scheduler`].
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::MissingAgent`] when the lookup returns `None`, or
    /// any failure reported by [`Self::run_scheduler`].
    pub async fn run_admitted_agents<'a>(
        self,
        max_concurrent: NonZeroUsize,
        cancellation: &CancellationToken,
        configure: impl FnMut(&ScheduledRun) -> Option<ScheduledRunConfig<'a>>,
        on_outcome: impl FnMut(&ScheduledRun, &RunOutcome),
    ) -> Result<(), SchedulerError> {
        self.drive_scheduler(true, max_concurrent, cancellation, configure, on_outcome)
            .await
    }

    async fn drive_scheduler<'a>(
        self,
        admitted_only: bool,
        max_concurrent: NonZeroUsize,
        cancellation: &CancellationToken,
        mut configure: impl FnMut(&ScheduledRun) -> Option<ScheduledRunConfig<'a>>,
        mut on_outcome: impl FnMut(&ScheduledRun, &RunOutcome),
    ) -> Result<(), SchedulerError> {
        let state = self.state.clone();
        let mut store = self;
        let (requests, commands) =
            mpsc::channel::<oneshot::Sender<Result<Schedule, JournalError>>>();
        thread::Builder::new()
            .name("wakefold-scheduler-db".into())
            .spawn(move || {
                while let Ok(reply) = commands.recv() {
                    let _ = reply.send(store.read_schedule(max_concurrent, admitted_only));
                }
            })
            .map_err(SchedulerError::StartWorker)?;

        let mut running = FuturesUnordered::new();
        let mut active = HashSet::new();
        let mut delay = MIN_IDLE_DELAY;
        while !cancellation.is_cancelled() {
            if active.len() < max_concurrent.get() {
                let (reply, response) = oneshot::channel();
                requests
                    .send(reply)
                    .map_err(|_| SchedulerError::WorkerStopped)?;
                let Either::Left((response, _)) =
                    select(response, pin!(cancellation.cancelled())).await
                else {
                    break;
                };
                let schedule = response.map_err(|_| SchedulerError::WorkerStopped)??;
                delay = schedule.delay;
                for ready in schedule.ready {
                    if active.insert(ready.run.agent_id) {
                        let config = configure(&ready.run)
                            .ok_or_else(|| SchedulerError::MissingAgent(ready.run.clone()))?;
                        running.push(resume(&state, ready, config));
                        if active.len() == max_concurrent.get() {
                            break;
                        }
                    }
                }
            }
            let next = async {
                if active.len() == max_concurrent.get() {
                    running.next().await
                } else if running.is_empty() {
                    Delay::new(delay).await;
                    None
                } else {
                    match select(running.next(), Delay::new(delay)).await {
                        Either::Left((result, _)) => result,
                        Either::Right(_) => None,
                    }
                }
            };
            let Either::Left((completed, _)) =
                select(pin!(next), pin!(cancellation.cancelled())).await
            else {
                break;
            };
            if let Some((run, result)) = completed {
                active.remove(&run.agent_id);
                if let Some(outcome) = result? {
                    on_outcome(&run, &outcome);
                }
            }
        }
        Ok(())
    }

    fn read_schedule(
        &mut self,
        limit: NonZeroUsize,
        admitted_only: bool,
    ) -> Result<Schedule, JournalError> {
        let now = SystemTime::now();
        let now_millis = current_millis(now);
        let next_timer: Option<i64> = self.connection.query_row(
            "SELECT MIN(wake_at_ms) FROM agent_runs
             WHERE wake_at_ms IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        let delay = next_timer.map_or(MAX_IDLE_DELAY, |deadline| {
            let millis = deadline.saturating_sub(now_millis).max(0) as u64;
            Duration::from_millis(millis).clamp(MIN_IDLE_DELAY, MAX_IDLE_DELAY)
        });
        if next_timer.is_some_and(|deadline| deadline <= now_millis) {
            self.wake_due_timers(now, limit)?;
        }
        let mut query = self.connection.prepare_cached(
            "SELECT r.agent_id, r.chat_id, r.run_key,
                    (SELECT event_json FROM run_events WHERE run_id=r.run_id ORDER BY sequence LIMIT 1),
                    s.settings_json, s.parent_agent_id, r.run_id
             FROM agent_runs r JOIN agent_sessions s USING(agent_id)
             WHERE r.status='runnable' AND (NOT ?2 OR s.settings_json IS NOT NULL)
             ORDER BY r.run_id LIMIT ?1",
        )?;
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let mut rows = query.query(rusqlite::params![limit, admitted_only])?;
        let mut ready = Vec::new();
        while let Some(row) = rows.next()? {
            let first: String = row.get(3)?;
            let crate::RunEvent::RunStarted(prompt) = serde_json::from_str(&first)? else {
                return Err(JournalError::InvalidWait(
                    "scheduled run is missing its original prompt",
                ));
            };
            ready.push(ReadyRun {
                run: ScheduledRun {
                    agent_id: row.get(0)?,
                    run_id: row.get(6)?,
                    chat_id: row.get(1)?,
                    run_key: row.get(2)?,
                    parent_agent_id: row.get(5)?,
                    settings: row
                        .get::<_, Option<String>>(4)?
                        .map(|json| serde_json::from_str(&json))
                        .transpose()?,
                },
                prompt,
            });
        }
        Ok(Schedule { ready, delay })
    }
}

async fn resume(
    state: &Path,
    ready: ReadyRun,
    config: ScheduledRunConfig<'_>,
) -> (ScheduledRun, Result<Option<RunOutcome>, SchedulerError>) {
    let ReadyRun { run, prompt } = ready;
    let result = async {
        let mut store = WorkspaceStore::open(state)?;
        let mut journal = store.open_journal(run.chat_id, &run.run_key)?;
        let outcome = journal
            .run_until_blocked(prompt, config.provider, config.skills, config.options)
            .await;
        match outcome {
            Ok(outcome) => Ok(Some(outcome)),
            Err(RunError::Journal(JournalError::AlreadyRunning { .. })) => {
                // A previous driver may still hold the lock after committing a wait.
                Delay::new(MAX_IDLE_DELAY).await;
                Ok(None)
            }
            Err(source) => Err(SchedulerError::Resume {
                run: run.clone(),
                source: Box::new(source),
            }),
        }
    }
    .await;
    (run, result)
}
