//! Opt-in network test. Run with `--features openai --test live_openai_skills -- --ignored`.
#![cfg(feature = "openai")]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use wakefold::{
    JsonValue, ModelRequest, ModelResponse, ModelTurn, OpenAiProvider, Provider, ProviderError,
    RetryPolicy, RunEvent, RunOptions, Skill, Tool, ToolError, ToolSpec, WorkspaceStore,
};

const PROMPT: &str = "Retrieve the pickup code for order WF-206 from the order records. Reply with just the pickup code.";
const CHILD_STATE: &str = "WAKEFOLD_LIVE_ORDER_TEST";

struct Lookup<'a>(&'a Path);

#[async_trait]
impl Tool for Lookup<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "lookup",
            "Retrieve the pickup code for one order ID",
            json!({
                "type": "object", "properties": {"order_id": {"type": "string"}},
                "required": ["order_id"], "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: &JsonValue) -> Result<JsonValue, ToolError> {
        if args["order_id"] != "WF-206" {
            return Err(ToolError::new("unknown order"));
        }
        let code = fs::read_to_string(self.0.join("pickup-code"))
            .map_err(|e| ToolError::with_source("read test fixture", e))?;
        Ok(json!({"pickup_code": code}))
    }
}

struct LiveProvider {
    inner: OpenAiProvider,
    stop_after_activation: bool,
}

#[async_trait]
impl Provider for LiveProvider {
    async fn complete(&self, req: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        Ok(self.complete_turn(req, &mut |_| {}).await?.response)
    }

    async fn complete_turn(
        &self,
        req: ModelRequest<'_>,
        on_text: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        if self.stop_after_activation && req.tools.iter().any(|spec| spec.name == "orders.lookup") {
            std::process::exit(73);
        }
        self.inner.complete_turn(req, on_text).await
    }
}

async fn drive(state: &Path, stop_after_activation: bool) -> Result<String, Box<dyn Error>> {
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    let inner = OpenAiProvider::new(model, std::env::var("OPENAI_API_KEY")?)?;
    let provider = LiveProvider {
        inner,
        stop_after_activation,
    };
    let mut store = WorkspaceStore::open(state)?;
    let mut journal = store.open_journal("synthetic-orders", "lookup")?;
    let tool = Lookup(state);
    let tools: [&dyn Tool; 1] = [&tool];
    let skills = [Skill::new(
        "orders",
        "Retrieve order records and pickup codes",
        &tools,
    )];
    let options = RunOptions::new(5)
        .with_token_budget(6_000)
        .with_retry_policy(RetryPolicy::new(0, Duration::ZERO));
    let report = tokio::time::timeout(
        Duration::from_secs(90),
        journal.run(PROMPT, &provider, &skills, options),
    )
    .await??;
    let entries = journal.load_after(0)?;
    assert_eq!(
        entries
            .iter()
            .filter(|entry| matches!(&entry.event, RunEvent::SkillLoaded(name) if name == "orders"))
            .count(),
        1
    );
    assert_eq!(entries.iter().filter(|entry| matches!(&entry.event, RunEvent::ToolCallStarted(call) if call.name == "orders.lookup")).count(), 1);
    Ok(report.answer)
}

#[tokio::test]
#[ignore = "uses a billable OpenAI model and requires OPENAI_API_KEY"]
async fn live_order_lookup_survives_a_process_restart() -> Result<(), Box<dyn Error>> {
    if let Some(state) = std::env::var_os(CHILD_STATE) {
        drive(Path::new(&state), true).await?;
        return Err("model finished before loading the orders skill".into());
    }
    let state = tempfile::tempdir()?;
    let code = format!("WF-{:016X}", rand::random::<u64>());
    fs::write(state.path().join("pickup-code"), &code)?;
    let status = Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "live_order_lookup_survives_a_process_restart",
            "--nocapture",
        ])
        .env(CHILD_STATE, state.path())
        .status()?;
    assert_eq!(
        status.code(),
        Some(73),
        "expected a committed skill activation in the first process"
    );
    assert_eq!(drive(state.path(), false).await?.trim(), code);
    Ok(())
}
