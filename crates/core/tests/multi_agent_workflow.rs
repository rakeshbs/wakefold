use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use wakefold::{
    CancellationToken, JsonValue, Message, ModelRequest, ModelResponse, ModelTurn, Provider,
    ProviderError, RunError, RunEvent, RunOptions, RunOutcome, ScheduledRunConfig, Skill,
    SpawnAgent, TokenUsage, Tool, ToolCall, ToolError, ToolGrant, ToolSpec, WaitForMessage, WaitOn,
    WakeEvent, WorkspaceStore,
};

const CHAT: wakefold::ChatId = wakefold::ChatId::from_u128(1);
const COORDINATOR: &str = "coordinator";
const PROMPT: &str = "Find the notebook price and obtain a review.";
const CATALOG: &str = "catalog";
const REVIEW: &str = "review";
const CATALOG_ANSWER: &str = "350 cents";
const REVIEW_ANSWER: &str = "order reviewed";
const FINAL_ANSWER: &str = "Notebook costs 350 cents and the order was reviewed.";
const CHILD_STATE: &str = "WAKEFOLD_WORKFLOW_TEST_STATE";
const CHILD_STOP: &str = "WAKEFOLD_WORKFLOW_TEST_STOP";

const COORDINATOR_LIMIT: u64 = 30;
const CATALOG_LIMIT: u64 = 9;
const REVIEW_LIMIT: u64 = 13;
const REVIEW_BEFORE_APPROVAL: u64 = 9;

fn coordinator_options() -> RunOptions {
    RunOptions::new(9)
        .with_token_budget(COORDINATOR_LIMIT)
        .with_tool_grants([
            ("agents.spawn", ToolGrant::Allow),
            ("mailbox.receive", ToolGrant::Allow),
        ])
}

fn worker_options(worker: &str) -> RunOptions {
    let (limit, tool, grant) = match worker {
        CATALOG => (CATALOG_LIMIT, "order.catalog", ToolGrant::Allow),
        REVIEW => (REVIEW_LIMIT, "order.review", ToolGrant::RequireApproval),
        _ => panic!("unexpected worker {worker}"),
    };
    RunOptions::new(4)
        .with_token_budget(limit)
        .with_tool_grants([(tool, grant)])
}

fn load(name: &str) -> ModelResponse {
    ToolCall::new("load", "load_skill", json!({ "name": name })).into()
}

fn dispatch(id: &str, name: &str, arguments: JsonValue) -> ToolCall {
    ToolCall::new(
        id,
        "call_tool",
        json!({ "tool_name": name, "arguments": arguments }),
    )
}

fn assert_result(message: &Message, id: &str, value: JsonValue) {
    let Message::Tool(result) = message else {
        panic!("expected tool result {id}")
    };
    assert_eq!(result.call_id, id.into());
    assert_eq!(result.value, value);
}

struct Coordinator;

#[async_trait]
impl Provider for Coordinator {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver uses model turns with usage")
    }

    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let (response, usage) = match request.messages.last().unwrap() {
            Message::Developer(_) => (load("agents"), TokenUsage::new(4, 1)),
            Message::Tool(result) => {
                let response = match result.call_id.as_str() {
                    "load" => ModelResponse::ToolCalls {
                        text: None,
                        calls: vec![
                            dispatch("catalog", "agents.spawn", json!({"task": CATALOG})),
                            dispatch("review", "agents.spawn", json!({"task": REVIEW})),
                        ],
                    },
                    "review" => {
                        ToolCall::new("mail", "load_skill", json!({"name":"mailbox"})).into()
                    }
                    "mail" | "catalog_result" => {
                        let (spawn, call) = if result.call_id.as_str() == "mail" {
                            (CATALOG, "catalog_result")
                        } else {
                            assert_eq!(result.value["payload"]["answer"], CATALOG_ANSWER);
                            (REVIEW, "review_result")
                        };
                        let child = request
                            .messages
                            .iter()
                            .find_map(|message| match message {
                                Message::Tool(r) if r.call_id.as_str() == spawn => {
                                    r.value["agent_id"].as_i64()
                                }
                                _ => None,
                            })
                            .unwrap();
                        dispatch(
                            call,
                            "mailbox.receive",
                            json!({"from_agent_id":child,"completion_only":true}),
                        )
                        .into()
                    }
                    "review_result" => {
                        assert_eq!(result.value["payload"]["answer"], REVIEW_ANSWER);
                        ModelResponse::Final(FINAL_ANSWER.into())
                    }
                    other => panic!("unexpected result {other}"),
                };
                (response, TokenUsage::new(2, 1))
            }
            other => panic!("unexpected coordinator boundary {other:?}"),
        };
        Ok(ModelTurn::new(response, Some(usage)))
    }
}

struct Worker {
    name: &'static str,
    answer: &'static str,
    token_counts: [(u64, u64); 3],
}

const CATALOG_WORKER: Worker = Worker {
    name: CATALOG,
    answer: CATALOG_ANSWER,
    token_counts: [(2, 1), (2, 1), (1, 1)],
};
const REVIEW_WORKER: Worker = Worker {
    name: REVIEW,
    answer: REVIEW_ANSWER,
    token_counts: [(3, 1), (4, 1), (2, 1)],
};

#[async_trait]
impl Provider for Worker {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver uses model turns with usage")
    }

    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let (response, index) = match request.messages.last().unwrap() {
            Message::Developer(_) => (load("order"), 0),
            Message::Tool(result) if result.call_id == "load".into() => {
                let name = format!("order.{}", self.name);
                let tools = result.value["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0]["name"], name);
                (
                    dispatch("work", &name, json!({ "item": "notebook" })).into(),
                    1,
                )
            }
            message @ Message::Tool(_) => {
                assert_result(message, "work", json!(self.answer));
                (ModelResponse::Final(self.answer.into()), 2)
            }
            other => panic!("unexpected worker boundary {other:?}"),
        };
        let (input, output) = self.token_counts[index];
        Ok(ModelTurn::new(
            response,
            Some(TokenUsage::new(input, output)),
        ))
    }
}

struct RecordWork<'a> {
    state: &'a Path,
    worker: &'a Worker,
}

#[async_trait]
impl Tool for RecordWork<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.worker.name,
            "Handle the order",
            json!({"type": "object"}),
        )
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        assert_eq!(*arguments, json!({ "item": "notebook" }));
        std::fs::File::create_new(self.state.join(self.worker.name))
            .expect("each worker's tool must run exactly once");
        Ok(json!(self.worker.answer))
    }
}

struct Workflow<'a> {
    catalog: RecordWork<'a>,
    review: RecordWork<'a>,
}

impl<'a> Workflow<'a> {
    fn new(state: &'a Path) -> Self {
        Self {
            catalog: RecordWork {
                state,
                worker: &CATALOG_WORKER,
            },
            review: RecordWork {
                state,
                worker: &REVIEW_WORKER,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopAt {
    PendingApproval,
    ReviewCompleted,
    CoordinatorCompleted,
}

async fn drive_scheduler(state: &Path, stop: StopAt) {
    let workflow = Workflow::new(state);
    let order_tools: [&dyn Tool; 2] = [&workflow.catalog, &workflow.review];
    let delegation_skills = [
        Skill::new("agents", "Spawn", &[&SpawnAgent]),
        Skill::new("mailbox", "Receive", &[&WaitForMessage]),
    ];
    let worker_skills = [Skill::new("order", "Handle an order", &order_tools)];
    let store = WorkspaceStore::open(state).unwrap();
    let tree = store.chat_runs(CHAT).unwrap();
    let worker = |agent_id| {
        tree.iter()
            .find(|r| r["agent_id"] == agent_id && r["parent_agent_id"].is_number())
            .and_then(|r| r["task"].as_str())
    };
    let cancellation = CancellationToken::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        store.run_scheduler(
            NonZeroUsize::MIN,
            &cancellation,
            |run| {
                let worker = worker(run.agent_id);
                match worker {
                    Some(CATALOG) if stop == StopAt::PendingApproval =>
                        ScheduledRunConfig::new(&CATALOG_WORKER, &worker_skills, worker_options(CATALOG)),
                    Some(REVIEW) if stop != StopAt::CoordinatorCompleted =>
                        ScheduledRunConfig::new(&REVIEW_WORKER, &worker_skills, worker_options(REVIEW)),
                    None => ScheduledRunConfig::new(&Coordinator, &delegation_skills, coordinator_options()),
                    _ => panic!("unexpected ready run {run:?} at {stop:?}"),
                }
            },
            |run, outcome| {
                let worker = worker(run.agent_id);
                match (stop, worker, outcome) {
                    (StopAt::PendingApproval, Some(CATALOG), RunOutcome::Finished(report)) => {
                        assert_eq!(report.answer, CATALOG_ANSWER);
                        assert_eq!(report.usage.reported_tokens, TokenUsage::new(5, 3));
                    }
                    (StopAt::PendingApproval, None, RunOutcome::Suspended(wait)) => {
                        assert!(matches!(&wait.on, WaitOn::ChildOutcome { .. }));
                    }
                    (StopAt::PendingApproval, Some(REVIEW), RunOutcome::Suspended(wait)) => {
                        assert!(matches!(&wait.on, WaitOn::ToolApproval { tool_name, .. } if tool_name == "order.review"));
                        std::process::exit(81);
                    }
                    (StopAt::ReviewCompleted, Some(REVIEW), RunOutcome::Finished(report)) => {
                        assert_eq!(report.answer, REVIEW_ANSWER);
                        assert_eq!(report.usage.reported_tokens, TokenUsage::new(9, 3));
                        std::process::exit(82);
                    }
                    (StopAt::CoordinatorCompleted, None, RunOutcome::Finished(report)) => {
                        assert_eq!(report.answer, FINAL_ANSWER);
                        assert_eq!(report.usage.reported_tokens, TokenUsage::new(14, 6));
                        cancellation.cancel();
                    }
                    _ => panic!("unexpected outcome for {run:?} at {stop:?}: {outcome:?}"),
                }
            },
        ),
    )
    .await
    .expect("workflow must reach the requested boundary")
    .unwrap();
}

fn crash_at(state: &Path, stop: &str, code: i32) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "coordinator_and_two_workers_survive_restarts_with_separate_token_budgets",
            "--nocapture",
        ])
        .env(CHILD_STATE, state)
        .env(CHILD_STOP, stop)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(code));
}

#[tokio::test]
async fn coordinator_and_two_workers_survive_restarts_with_separate_token_budgets() {
    if let Some(state) = std::env::var_os(CHILD_STATE) {
        let stop = match std::env::var(CHILD_STOP).unwrap().as_str() {
            "approval" => StopAt::PendingApproval,
            "review" => StopAt::ReviewCompleted,
            other => panic!("unknown boundary {other}"),
        };
        drive_scheduler(Path::new(&state), stop).await;
        panic!("expected the scheduler process to exit at {stop:?}");
    }

    let state = tempfile::tempdir().unwrap();
    let path = state.path();
    let workflow = Workflow::new(path);
    let skills = [
        Skill::new("agents", "Spawn", &[&SpawnAgent]),
        Skill::new("mailbox", "Receive", &[&WaitForMessage]),
    ];
    let mut store = WorkspaceStore::open(path).unwrap();
    let settings =
        serde_json::from_value(json!({"model":"scripted","max_model_turns":9,"token_budget":30}))
            .unwrap();
    store
        .queue_root(CHAT, COORDINATOR, PROMPT, &settings, None)
        .unwrap();
    let outcome = store
        .open_journal(CHAT, COORDINATOR)
        .unwrap()
        .run_until_blocked(PROMPT, &Coordinator, &skills, coordinator_options())
        .await
        .unwrap();
    let RunOutcome::Suspended(first_wait) = outcome else {
        panic!("expected the catalog worker")
    };
    assert!(matches!(&first_wait.on, WaitOn::ChildOutcome { .. }));
    drop(store);

    crash_at(path, "approval", 81);
    assert!(path.join(CATALOG).is_file());
    assert!(
        !path.join(REVIEW).exists(),
        "the reviewer must wait for approval"
    );

    let mut store = WorkspaceStore::open(path).unwrap();
    let approvals = store.pending_tool_approvals().unwrap();
    let [(review_id, approval)] = approvals.as_slice() else {
        panic!("expected one saved approval")
    };
    assert!(
        matches!(&approval.on, WaitOn::ToolApproval { tool_name, arguments }
        if tool_name == "order.review" && *arguments == json!({ "item": "notebook" }))
    );
    let tree = store.chat_runs(CHAT).unwrap();
    let run_key = |task| {
        tree.iter()
            .find(|r| r["task"].as_str() == Some(task))
            .unwrap()["run_key"]
            .as_str()
            .unwrap()
    };
    let review_key = run_key(REVIEW);
    let review_workspace = store.open_run_workspace(CHAT, review_key).unwrap();
    assert_eq!(*review_id, review_workspace.agent_id());
    assert!(
        store
            .resolve_wait(&approval.id, WakeEvent::Approval { approved: true })
            .unwrap()
    );
    assert!(
        !store
            .resolve_wait(&approval.id, WakeEvent::Approval { approved: true })
            .unwrap()
    );

    // A cap equal to the reviewer's already-saved usage must block the approved tool.
    let order_tools: [&dyn Tool; 2] = [&workflow.catalog, &workflow.review];
    let worker_skills = [Skill::new("order", "Handle an order", &order_tools)];
    let limited = store
        .open_journal(CHAT, review_key)
        .unwrap()
        .run_until_blocked(
            REVIEW,
            &REVIEW_WORKER,
            &worker_skills,
            worker_options(REVIEW).with_token_budget(REVIEW_BEFORE_APPROVAL),
        )
        .await;
    assert!(
        matches!(limited, Err(RunError::TokenBudgetReached { limit: REVIEW_BEFORE_APPROVAL, usage })
        if usage.reported_tokens == TokenUsage::new(7, 2))
    );
    assert!(!path.join(REVIEW).exists());
    drop(store);

    crash_at(path, "review", 82);
    assert!(path.join(REVIEW).is_file());
    drive_scheduler(path, StopAt::CoordinatorCompleted).await;

    let mut store = WorkspaceStore::open(path).unwrap();
    for key in [COORDINATOR, run_key(CATALOG), review_key] {
        let history = store
            .open_journal(CHAT, key)
            .unwrap()
            .load_after(0)
            .unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|entry| matches!(entry.event, RunEvent::ModelCallCompleted(_)))
                .count(),
            if key == COORDINATOR { 6 } else { 3 },
            "{key}"
        );
        assert_eq!(
            history
                .iter()
                .filter(|entry| matches!(entry.event, RunEvent::RunFinished(_)))
                .count(),
            1,
            "{key}"
        );
    }
    let parent = store
        .open_journal(CHAT, COORDINATOR)
        .unwrap()
        .load_after(0)
        .unwrap();
    assert_eq!(
        parent
            .iter()
            .filter(|entry| matches!(entry.event, RunEvent::Woken { .. }))
            .count(),
        2
    );
    assert!(store.pending_tool_approvals().unwrap().is_empty());
}
