//! Daemon lifecycle and scheduler integration.

use std::collections::BTreeMap;
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
    mut runtime: Runtime,
    stop: CancellationToken,
) -> Result<(), DaemonError> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err("daemon listener must be loopback".into());
    }
    let store = WorkspaceStore::open(&runtime.state_directory)?;
    tracing::info!(address = %listener.local_addr()?, "daemon started");
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
    let sandbox_maintenance = async {
        #[cfg(feature = "sandbox")]
        if let Some(sandbox) = &runtime.sandbox {
            sandbox.maintain(&stop).await;
        }
    };
    let (http, scheduler, ()) = tokio::join!(http, scheduler, sandbox_maintenance);
    for (name, server) in &mut runtime.mcp_servers {
        server.close(name).await;
    }
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
                    tracing::info!(
                        agent_id = run.agent_id,
                        parent_agent_id =
                            run.delegation.as_ref().map(|worker| worker.parent_agent_id),
                        agent = name,
                        chat_id = %run.chat_id,
                        run_key = run.run_key.as_str(),
                        "dispatching ready run"
                    );
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
        tracing::warn!(
            agent_id = run.agent_id,
            chat_id = %run.chat_id,
            run_key = run.run_key.as_str(),
            "run execution failed"
        );
        fail_run(&runtime.state_directory, &run, error)?;
    }
    Ok(())
}

fn fail_run(state: &Path, run: &ScheduledRun, error: String) -> Result<(), DaemonError> {
    let mut store = WorkspaceStore::open(state)?;
    store
        .open_journal(run.chat_id, &run.run_key)?
        .record_failure(error)?;
    Ok(())
}
