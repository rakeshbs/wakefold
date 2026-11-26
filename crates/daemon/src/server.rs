//! Daemon lifecycle and scheduler integration.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use tokio::net::TcpListener;
use wakefold::{
    CancellationToken, ScheduledRun, ScheduledRunConfig, SchedulerError, Skill, Tool,
    WorkspaceStore,
};

use crate::config::Runtime;
use crate::{DaemonError, api};

pub(crate) async fn serve(
    listener: TcpListener,
    runtime: Runtime,
    stop: CancellationToken,
) -> Result<(), DaemonError> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err("daemon listener must be loopback".into());
    }
    let store = WorkspaceStore::open(&runtime.state_directory)?;
    // A single daemon owns the scheduler and handles terminal run failures.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(runtime.state_directory.join("daemon.lock"))?;
    lock.try_lock()
        .map_err(|error| format!("could not lock Wakefold state directory: {error}"))?;
    let app = api::router(
        store,
        runtime.agents.keys().cloned().collect(),
        stop.clone(),
    );
    let http = async {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(stop.clone().cancelled_owned())
            .await;
        stop.cancel();
        result
    };
    let scheduler = async {
        let result = drive(&runtime, &stop).await;
        stop.cancel();
        result
    };
    let (http, scheduler) = tokio::join!(http, scheduler);
    scheduler?;
    http?;
    Ok(())
}

async fn drive(runtime: &Runtime, stop: &CancellationToken) -> Result<(), DaemonError> {
    let tools: BTreeMap<_, Vec<&dyn Tool>> = runtime
        .skills
        .iter()
        .map(|(name, skill)| (name.as_str(), skill.tools.iter().map(Box::as_ref).collect()))
        .collect();
    let agent_skills: BTreeMap<_, Vec<Skill<'_>>> = runtime
        .agents
        .iter()
        .map(|(name, agent)| {
            let skills = agent
                .skills
                .iter()
                .map(|skill| {
                    Skill::new(
                        skill,
                        &runtime.skills[skill].description,
                        &tools[skill.as_str()],
                    )
                })
                .collect();
            (name.as_str(), skills)
        })
        .collect();
    while !stop.is_cancelled() {
        let scheduler = WorkspaceStore::open(&runtime.state_directory)?;
        let result = scheduler
            .run_scheduler_with_agent_lookup(
                runtime.max_concurrent,
                stop,
                |run| {
                    let name = run.agent_name.as_deref()?;
                    let agent = runtime.agents.get(name)?;
                    Some(ScheduledRunConfig::new(
                        agent.provider.as_ref(),
                        &agent_skills[name],
                        agent.options.clone(),
                    ))
                },
                |_, _| {},
            )
            .await;
        let (run, error) = match result {
            Ok(()) => return Ok(()),
            Err(SchedulerError::Resume { run, source }) => (run, source.to_string()),
            Err(SchedulerError::MissingAgent(run)) => {
                let message = format!("no agent configuration for run {}", run.run_key);
                (run, message)
            }
            Err(error) => return Err(error.into()),
        };
        eprintln!(
            "Wakefold run {} in chat {} failed: {error}",
            run.run_key, run.chat_key
        );
        fail_run(&runtime.state_directory, &run, error)?;
    }
    Ok(())
}

fn fail_run(state: &Path, run: &ScheduledRun, error: String) -> Result<(), DaemonError> {
    let mut store = WorkspaceStore::open(state)?;
    store
        .open_journal(&run.chat_key, &run.run_key)?
        .record_failure(error)?;
    Ok(())
}
