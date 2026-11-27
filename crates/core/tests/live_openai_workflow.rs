//! Opt-in, billable end-to-end test of a real model with local order tools.
#![cfg(feature = "openai")]

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use wakefold::{
    CancellationToken, DelegateTool, JournalEntry, JsonValue, ModelRequest, ModelResponse,
    ModelRetryPolicy, OpenAiProvider, Provider, ProviderError, ReasoningEffort, RunError, RunEvent,
    RunOptions, RunOutcome, ScheduledRunConfig, Skill, Tool, ToolError, ToolGrant, ToolSpec,
    WaitOn, WakeEvent, WorkspaceStore,
};

const STATE: &str = "WAKEFOLD_LIVE_WORKFLOW_STATE";
const STAGE: &str = "WAKEFOLD_LIVE_WORKFLOW_STAGE";
const CHAT: wakefold::ChatId = wakefold::ChatId::from_u128(1);
const COORDINATOR: &str = "coordinator";
const CATALOG: &str = "catalog";
const REVIEW: &str = "review";
const SKU: &str = "WF-318";
const COORDINATOR_BUDGET: u64 = 12_000;
const CATALOG_BUDGET: u64 = 4_000;
const REVIEW_BUDGET: u64 = 6_000;
const PROMPT: &str = "+Process order WF-318. Use the tools to get a catalog price and an approved review receipt.
First call load_skill with name delegate. Then call delegate.catalog with a task asking the worker +to load the orders skill, call orders.lookup_price for sku WF-318, and return the price in cents +as digits only. Wait for its result.
Next call delegate.review with a task asking the worker to load the orders skill, call +orders.record_review for sku WF-318, and return the receipt ID only. Wait for its result.
After both worker results, reply with one JSON object with an integer price_cents and a string +receipt. Use the actual returned values and omit Markdown fences.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    AwaitingApproval,
    ReviewFinished,
    FinishOrder,
}

#[derive(Clone, Copy)]
enum Action {
    LookUpPrice,
    RecordReview,
}

struct OrderTool<'a> {
    state: &'a Path,
    action: Action,
}

#[async_trait]
impl Tool for OrderTool<'_> {
    fn spec(&self) -> ToolSpec {
        let (name, description) = match self.action {
            Action::LookUpPrice => (
                "lookup_price",
                "Look up the current price in cents for the SKU.",
            ),
            Action::RecordReview => (
                "record_review",
                "Record an approved order review and return its receipt ID.",
            ),
        };
        ToolSpec::new(
            name,
            description,
            json!({
                "type": "object",
                "properties": { "sku": { "type": "string", "enum": [SKU] } },
                "required": ["sku"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        if *arguments != json!({ "sku": SKU }) {
            return Err(ToolError::new("expected the test order SKU"));
        }
        let io_error =
            |error| ToolError::with_source("local order fixture operation failed", error);
        match self.action {
            Action::LookUpPrice => {
                let catalog =
                    fs::read_to_string(self.state.join("catalog.json")).map_err(io_error)?;
                let catalog: JsonValue = serde_json::from_str(&catalog)
                    .map_err(|error| ToolError::with_source("invalid catalog fixture", error))?;
                File::create_new(self.state.join("catalog-read")).map_err(io_error)?;
                Ok(catalog)
            }
            Action::RecordReview => {
                let receipt = format!("REV-{:016X}", rand::random::<u64>());
                let record = json!({ "sku": SKU, "receipt": receipt });
                let mut file =
                    File::create_new(self.state.join("review.json")).map_err(io_error)?;
                file.write_all(record.to_string().as_bytes())
                    .map_err(io_error)?;
                file.sync_all().map_err(io_error)?;
                Ok(record)
            }
        }
    }
}

struct Delegates {
    catalog: DelegateTool,
    review: DelegateTool,
}

impl Delegates {
    fn new() -> Self {
        Self {
            catalog: DelegateTool::new(
                CATALOG,
                "Ask the catalog worker for the item's price.",
                CATALOG,
            ),
            review: DelegateTool::new(
                REVIEW,
                "Ask the reviewer to record an approved review.",
                REVIEW,
            ),
        }
    }
}

fn live_provider() -> Result<OpenAiProvider, Box<dyn Error>> {
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    Ok(
        OpenAiProvider::new(model, std::env::var("OPENAI_API_KEY")?)?
            .with_reasoning_effort(ReasoningEffort::None),
    )
}

fn options(role: &str) -> RunOptions {
    let base = RunOptions::new(6).with_model_retry_policy(ModelRetryPolicy::new(0, Duration::ZERO));
    match role {
        COORDINATOR => base
            .with_token_budget(COORDINATOR_BUDGET)
            .with_tool_grants([
                ("delegate.catalog", ToolGrant::Allow),
                ("delegate.review", ToolGrant::Allow),
            ]),
        CATALOG => base
            .with_token_budget(CATALOG_BUDGET)
            .with_tool_grants([("orders.lookup_price", ToolGrant::Allow)]),
        REVIEW => base
            .with_token_budget(REVIEW_BUDGET)
            .with_tool_grants([("orders.record_review", ToolGrant::RequireApproval)]),
        _ => panic!("unknown role {role}"),
    }
}

async fn schedule(state: &Path, stage: Stage) -> Result<String, Box<dyn Error>> {
    let provider = live_provider()?;
    let delegates = Delegates::new();
    let lookup = OrderTool {
        state,
        action: Action::LookUpPrice,
    };
    let record = OrderTool {
        state,
        action: Action::RecordReview,
    };
    let delegate_tools: [&dyn Tool; 2] = [&delegates.catalog, &delegates.review];
    let worker_tools: [&dyn Tool; 2] = [&lookup, &record];
    let parent_skills = [Skill::new(
        "delegate",
        "Delegate order work",
        &delegate_tools,
    )];
    let worker_skills = [Skill::new(
        "orders",
        "Read catalog or record a review",
        &worker_tools,
    )];
    let cancel = CancellationToken::new();
    let mut answer = None;
    let store = WorkspaceStore::open(state)?;
    tokio::time::timeout(Duration::from_secs(90), store.run_scheduler(
        NonZeroUsize::MIN,
        &cancel,
        |run| {
            let worker = run.delegation.as_ref().map(|delegation| delegation.worker.as_str());
            let role = match (stage, worker) {
                (Stage::AwaitingApproval, Some(CATALOG)) => CATALOG,
                (Stage::AwaitingApproval | Stage::ReviewFinished, Some(REVIEW)) => REVIEW,
                (Stage::AwaitingApproval | Stage::FinishOrder, None) => COORDINATOR,
                _ => panic!("unexpected ready run {run:?} in {stage:?}"),
            };
            let skills = if worker.is_some() { &worker_skills } else { &parent_skills };
            ScheduledRunConfig::new(&provider, skills, options(role))
        },
        |run, outcome| {
            let worker = run.delegation.as_ref().map(|delegation| delegation.worker.as_str());
            match (stage, worker, outcome) {
                (Stage::AwaitingApproval, Some(CATALOG), RunOutcome::Finished(report)) => {
                    let count = report.usage.reported_tokens.total_tokens();
                    assert!(count > 0 && count < CATALOG_BUDGET.into());
                    eprintln!("Catalog worker used {count}/{CATALOG_BUDGET} tokens");
                }
                (Stage::AwaitingApproval, None, RunOutcome::Suspended(wait)) => {
                    assert!(matches!(&wait.on, WaitOn::Delegation { worker, .. } if worker == REVIEW));
                }
                (Stage::AwaitingApproval, Some(REVIEW), RunOutcome::Suspended(wait)) => {
                    assert!(matches!(&wait.on, WaitOn::ToolApproval { tool_name, .. } if tool_name == "orders.record_review"));
                    eprintln!("Stopping with the review approval durably recorded");
                    std::process::exit(83);
                }
                (Stage::ReviewFinished, Some(REVIEW), RunOutcome::Finished(report)) => {
                    let count = report.usage.reported_tokens.total_tokens();
                    assert!(count > 0 && count < REVIEW_BUDGET.into());
                    eprintln!("Review worker used {count}/{REVIEW_BUDGET} tokens");
                    eprintln!("Stopping after review completion committed the parent wake");
                    std::process::exit(84);
                }
                (Stage::FinishOrder, None, RunOutcome::Finished(report)) => {
                    let count = report.usage.reported_tokens.total_tokens();
                    assert!(count > 0 && count < COORDINATOR_BUDGET.into());
                    eprintln!("Coordinator used {count}/{COORDINATOR_BUDGET} tokens");
                    answer = Some(report.answer.clone());
                    cancel.cancel();
                }
                _ => panic!("unexpected {outcome:?} from {run:?} in {stage:?}"),
            }
        },
    )).await??;
    answer.ok_or_else(|| "scheduler stopped before returning the completed order".into())
}

fn stop_child(state: &Path, stage: &str, exit_code: i32) -> Result<(), Box<dyn Error>> {
    let status = Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "live_order_workflow_survives_two_restarts",
            "--nocapture",
        ])
        .env(STATE, state)
        .env(STAGE, stage)
        .status()?;
    assert_eq!(status.code(), Some(exit_code), "did not reach {stage}");
    Ok(())
}

fn delegated_run<'a>(entries: &'a [JournalEntry], worker_name: &str) -> (&'a str, &'a str) {
    entries
        .iter()
        .find_map(|entry| match &entry.event {
            RunEvent::WaitStarted(wait) => match &wait.on {
                WaitOn::Delegation { worker, prompt } if worker == worker_name => {
                    Some((wait.id.as_str(), prompt.as_str()))
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("no saved delegation for {worker_name}"))
}

struct ForbidModelCall;

#[async_trait]
impl Provider for ForbidModelCall {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        panic!("the budget probe must not issue a model request")
    }
}

#[tokio::test]
#[ignore = "uses a billable OpenAI model and requires OPENAI_API_KEY"]
async fn live_order_workflow_survives_two_restarts() -> Result<(), Box<dyn Error>> {
    if let Some(state) = std::env::var_os(STATE) {
        let stage = match std::env::var(STAGE)?.as_str() {
            "approval" => Stage::AwaitingApproval,
            "review" => Stage::ReviewFinished,
            value => panic!("unknown stage {value}"),
        };
        return schedule(Path::new(&state), stage).await.map(|_| ());
    }

    let state = tempfile::tempdir()?;
    let path = state.path();
    let price: u64 = 1_000 + u64::from(rand::random::<u16>() % 5_000);
    fs::write(
        path.join("catalog.json"),
        json!({ "sku": SKU, "price_cents": price }).to_string(),
    )?;
    let provider = live_provider()?;
    let delegates = Delegates::new();
    let tools: [&dyn Tool; 2] = [&delegates.catalog, &delegates.review];
    let skills = [Skill::new("delegate", "Delegate order work", &tools)];
    let mut store = WorkspaceStore::open(path)?;
    let outcome = tokio::time::timeout(
        Duration::from_secs(45),
        store.open_journal(CHAT, COORDINATOR)?.run_until_blocked(
            PROMPT,
            &provider,
            &skills,
            options(COORDINATOR),
        ),
    )
    .await??;
    assert!(
        matches!(outcome, RunOutcome::Suspended(wait) if matches!(&wait.on, WaitOn::Delegation { worker, .. } if worker == CATALOG))
    );
    drop(store);

    stop_child(path, "approval", 83)?;
    assert!(path.join("catalog-read").exists());
    assert!(
        !path.join("review.json").exists(),
        "review must not execute before approval"
    );
    let mut store = WorkspaceStore::open(path)?;
    let approvals = store.pending_tool_approvals()?;
    let [(review_id, approval)] = approvals.as_slice() else {
        panic!("expected exactly one approval")
    };
    assert!(
        matches!(&approval.on, WaitOn::ToolApproval { tool_name, arguments }
        if tool_name == "orders.record_review" && arguments == &json!({ "sku": SKU }))
    );
    let parent = store.open_journal(CHAT, COORDINATOR)?.load_after(0)?;
    let (review_key, review_prompt) = delegated_run(&parent, REVIEW);
    assert_eq!(
        store.open_run_workspace(CHAT, review_key)?.agent_id(),
        *review_id
    );
    assert!(store.resolve_wait(&approval.id, WakeEvent::Approval { approved: true })?);

    let spent: u128 = store
        .open_journal(CHAT, review_key)?
        .load_after(0)?
        .iter()
        .filter_map(|entry| match &entry.event {
            RunEvent::ModelCallCompleted(turn) => {
                Some(turn.usage.expect("OpenAI must report usage").total_tokens())
            }
            _ => None,
        })
        .sum();
    assert!(spent > 0 && spent < REVIEW_BUDGET.into());
    let record = OrderTool {
        state: path,
        action: Action::RecordReview,
    };
    let worker_tools: [&dyn Tool; 1] = [&record];
    let worker_skills = [Skill::new(
        "orders",
        "Read catalog or record a review",
        &worker_tools,
    )];
    let cap = u64::try_from(spent)?;
    let limited = store
        .open_journal(CHAT, review_key)?
        .run_until_blocked(
            review_prompt,
            &ForbidModelCall,
            &worker_skills,
            options(REVIEW).with_token_budget(cap),
        )
        .await;
    assert!(
        matches!(limited, Err(RunError::TokenBudgetReached { limit, usage })
        if limit == cap && usage.reported_tokens.total_tokens() == spent)
    );
    assert!(!path.join("review.json").exists());
    eprintln!("The saved {cap} review tokens blocked dispatch at the same cap after restart");
    drop(store);

    stop_child(path, "review", 84)?;
    let record: JsonValue = serde_json::from_str(&fs::read_to_string(path.join("review.json"))?)?;
    let answer: JsonValue = serde_json::from_str(&schedule(path, Stage::FinishOrder).await?)?;
    assert_eq!(
        answer,
        json!({ "price_cents": price, "receipt": record["receipt"] })
    );
    let mut store = WorkspaceStore::open(path)?;
    let parent = store.open_journal(CHAT, COORDINATOR)?.load_after(0)?;
    assert_eq!(
        parent
            .iter()
            .filter(|entry| matches!(&entry.event, RunEvent::Woken { .. }))
            .count(),
        2
    );
    assert!(store.pending_tool_approvals()?.is_empty());
    Ok(())
}
