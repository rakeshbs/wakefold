use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    CancellationToken, DelegateTool, JsonValue, Message, ModelRequest, ModelResponse, Provider,
    ProviderError, RunError, RunEvent, RunOptions, RunOutcome, ScheduledRunConfig, Skill, Tool,
    ToolCall, ToolError, ToolGrant, ToolSpec, WaitOn, WakeEvent, WorkspaceStore,
};

const TASK: &str = "Find the notebook price";
const PROMPT: &str = "Ask the researcher for the notebook price.";
const CHILD_STATE: &str = "WAKEFOLD_DELEGATION_TEST_STATE";

struct Coordinator;

#[async_trait]
impl Provider for Coordinator {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = match request.messages.last().unwrap() {
            Message::User(prompt) => {
                assert_eq!(prompt, PROMPT);
                ToolCall::new("load", "load_skill", json!({"name": "delegate"})).into()
            }
            Message::Tool(result) if result.call_id == "load".into() => ToolCall::new(
                "research-call",
                "call_tool",
                json!({"tool_name": "delegate.research", "arguments": {"task": TASK}}),
            )
            .into(),
            Message::Tool(result) => {
                assert_eq!(result.call_id, "research-call".into());
                assert_eq!(
                    result.value,
                    json!({"worker": "researcher", "answer": "350 cents"})
                );
                ModelResponse::Final("The notebook costs 350 cents.".into())
            }
            other => panic!("unexpected coordinator boundary {other:?}"),
        };
        Ok(response)
    }
}

struct FinishWorker<'a>(&'a Path);

#[async_trait]
impl Provider for FinishWorker<'_> {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        assert_eq!(request.messages, [Message::User(TASK.into())]);
        std::fs::File::create_new(self.0.join("research-ran"))
            .expect("research ran more than once");
        Ok(ModelResponse::Final("350 cents".into()))
    }
}

fn delegate_tool() -> DelegateTool {
    DelegateTool::new(
        "research",
        "Delegate research to the researcher",
        "researcher",
    )
}

fn finish_worker_then_exit(path: &Path) {
    let store = WorkspaceStore::open(path).unwrap();
    let cancellation = CancellationToken::new();
    let worker = FinishWorker(path);
    block_on(store.run_scheduler(
        NonZeroUsize::MIN,
        &cancellation,
        |run| {
            let delegation = run
                .delegation
                .as_ref()
                .expect("only the worker should be ready");
            assert_eq!(delegation.worker, "researcher");
            assert_eq!(run.chat_key, "chat");
            assert_ne!(delegation.parent_agent_id, run.agent_id);
            ScheduledRunConfig::new(&worker, &[], RunOptions::new(1))
        },
        |_, outcome| {
            assert!(
                matches!(outcome, RunOutcome::Finished(report) if report.answer == "350 cents")
            );
            // Child completion and the parent's wake are already committed.
            std::process::exit(75);
        },
    ))
    .unwrap();
    panic!("worker did not finish");
}

#[test]
fn restart_delivers_the_saved_worker_result_once() {
    if let Some(path) = std::env::var_os(CHILD_STATE) {
        finish_worker_then_exit(Path::new(&path));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    let mut store = WorkspaceStore::open(path).unwrap();
    let parent = store.open_run_workspace("chat", "parent").unwrap();
    let tool = delegate_tool();
    let tools: [&dyn Tool; 1] = [&tool];
    let skills = [Skill::new("delegate", "Delegate a task", &tools)];
    let outcome = block_on(
        store
            .open_journal("chat", "parent")
            .unwrap()
            .run_until_blocked(PROMPT, &Coordinator, &skills, RunOptions::new(3)),
    )
    .unwrap();
    let RunOutcome::Suspended(wait) = outcome else {
        panic!("coordinator should wait for the worker");
    };
    assert!(
        matches!(wait.on, WaitOn::Delegation { worker, prompt } if worker == "researcher" && prompt == TASK)
    );
    drop(store);

    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "restart_delivers_the_saved_worker_result_once"])
        .env(CHILD_STATE, path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(75));
    assert!(path.join("research-ran").is_file());

    let mut store = WorkspaceStore::open(path).unwrap();
    let child = store.open_run_workspace("chat", &wait.id).unwrap();
    assert_eq!(child.chat_root(), parent.chat_root());
    assert_ne!(
        child.agent_relative_directory(),
        parent.agent_relative_directory()
    );
    let events = store
        .open_journal("chat", "parent")
        .unwrap()
        .load_after(0)
        .unwrap();
    assert!(
        matches!(&events.last().unwrap().event, RunEvent::Woken { wait_id, .. } if wait_id == &wait.id)
    );
    let cancellation = CancellationToken::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), store.run_scheduler(
            NonZeroUsize::MIN, &cancellation,
            |run| {
                assert_eq!(run.agent_id, parent.agent_id(), "the worker must never be queued again");
                assert!(run.delegation.is_none());
                ScheduledRunConfig::new(&Coordinator, &skills, RunOptions::new(3))
            },
            |_, outcome| {
                assert!(matches!(outcome, RunOutcome::Finished(report) if report.answer == "The notebook costs 350 cents."));
                cancellation.cancel();
            },
        )).await.unwrap().unwrap();
    });
    let mut store = WorkspaceStore::open(path).unwrap();
    let events = store
        .open_journal("chat", "parent")
        .unwrap()
        .load_after(0)
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|entry| matches!(&entry.event, RunEvent::Woken { .. }))
            .count(),
        1
    );
    assert!(matches!(
        &events.last().unwrap().event,
        RunEvent::RunFinished(_)
    ));
}

struct LookupPrice<'a> {
    path: &'a Path,
    name: &'static str,
}

#[async_trait]
impl Tool for LookupPrice<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name, "Look up a price", json!({"type": "object"}))
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        assert_eq!(*arguments, json!({"sku": "notebook"}));
        std::fs::File::create_new(self.path.join(self.name)).unwrap();
        Ok(json!("350 cents"))
    }
}

struct Researcher {
    tool: &'static str,
    expected: JsonValue,
}

#[async_trait]
impl Provider for Researcher {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = match request.messages.last().unwrap() {
            Message::User(prompt) => {
                assert_eq!(prompt, TASK);
                assert!(!request.tools[0].description.contains("admin:"));
                ToolCall::new("load", "load_skill", json!({"name": "prices"})).into()
            }
            Message::Tool(result) if result.call_id == "load".into() => {
                let tools = result.value["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0]["name"], "prices.lookup");
                ToolCall::new(
                    "price",
                    "call_tool",
                    json!({"tool_name": self.tool, "arguments": {"sku": "notebook"}}),
                )
                .into()
            }
            Message::Tool(result) => {
                assert_eq!(result.call_id, "price".into());
                assert_eq!(result.value, self.expected);
                ModelResponse::Final(result.value.as_str().unwrap_or("declined").into())
            }
            other => panic!("unexpected researcher boundary {other:?}"),
        };
        Ok(response)
    }
}

fn research_options(grant: ToolGrant) -> RunOptions {
    RunOptions::new(3).with_tool_grants([("prices.lookup", grant)])
}

#[test]
fn ungranted_tool_is_hidden_and_cannot_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let lookup = LookupPrice {
        path: root.path(),
        name: "lookup",
    };
    let private = LookupPrice {
        path: root.path(),
        name: "private",
    };
    let tools: [&dyn Tool; 2] = [&lookup, &private];
    let skills = [
        Skill::new("prices", "Prices", &tools),
        Skill::new("admin", "Restricted tools", &tools),
    ];
    let denied = block_on(wakefold::run_with_options(
        TASK,
        &Researcher {
            tool: "prices.private",
            expected: JsonValue::Null,
        },
        &skills,
        research_options(ToolGrant::Allow),
    ));
    assert!(matches!(denied, Err(RunError::ToolAccessDenied { name }) if name == "prices.private"));
    assert!(!root.path().join("private").exists());
}

#[test]
fn declined_approval_finishes_the_call_without_running_the_handler() {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(root.path()).unwrap();
    let lookup = LookupPrice {
        path: root.path(),
        name: "lookup",
    };
    let tools: [&dyn Tool; 1] = [&lookup];
    let skills = [Skill::new("prices", "Prices", &tools)];
    let provider = Researcher {
        tool: "prices.lookup",
        expected: json!({"approved": false}),
    };
    let pending = block_on(
        store
            .open_journal("chat", "run")
            .unwrap()
            .run_until_blocked(
                TASK,
                &provider,
                &skills,
                research_options(ToolGrant::RequireApproval),
            ),
    )
    .unwrap();
    let RunOutcome::Suspended(wait) = pending else {
        panic!("expected an approval request")
    };
    assert!(
        store
            .resolve_wait(&wait.id, WakeEvent::Approval { approved: false })
            .unwrap()
    );
    assert!(
        !store
            .resolve_wait(&wait.id, WakeEvent::Approval { approved: true })
            .unwrap()
    );
    let outcome = block_on(
        store
            .open_journal("chat", "run")
            .unwrap()
            .run_until_blocked(
                TASK,
                &provider,
                &skills,
                research_options(ToolGrant::RequireApproval),
            ),
    )
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Finished(report) if report.answer == "declined"));
    assert!(!root.path().join("lookup").exists());
}

#[tokio::test]
async fn delegated_worker_resumes_after_saved_tool_approval() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    let mut store = WorkspaceStore::open(path).unwrap();
    let delegate = delegate_tool();
    let delegation_tools: [&dyn Tool; 1] = [&delegate];
    let coordinator_skills = [Skill::new("delegate", "Delegate a task", &delegation_tools)];
    let coordinator_options =
        || RunOptions::new(3).with_tool_grants([("delegate.research", ToolGrant::Allow)]);
    let lookup = LookupPrice {
        path,
        name: "lookup",
    };
    let worker_tools: [&dyn Tool; 1] = [&lookup];
    let worker_skills = [Skill::new("prices", "Prices", &worker_tools)];
    let worker = Researcher {
        tool: "prices.lookup",
        expected: json!("350 cents"),
    };
    let outcome = store
        .open_journal("chat", "parent")
        .unwrap()
        .run_until_blocked(
            PROMPT,
            &Coordinator,
            &coordinator_skills,
            coordinator_options(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, RunOutcome::Suspended(_)));

    let cancellation = CancellationToken::new();
    let mut worker_id = None;
    tokio::time::timeout(Duration::from_secs(5), store.run_scheduler(
        NonZeroUsize::MIN, &cancellation,
        |run| {
            assert_eq!(run.delegation.as_ref().unwrap().worker, "researcher");
            ScheduledRunConfig::new(&worker, &worker_skills, research_options(ToolGrant::RequireApproval))
        },
        |run, outcome| {
            assert!(matches!(outcome, RunOutcome::Suspended(wait) if matches!(wait.on, WaitOn::ToolApproval { .. })));
            worker_id = Some(run.agent_id);
            cancellation.cancel();
        },
    )).await.unwrap().unwrap();
    assert!(
        !path.join("lookup").exists(),
        "the worker must wait before executing"
    );

    let mut store = WorkspaceStore::open(path).unwrap();
    let approvals = store.pending_tool_approvals().unwrap();
    let [(agent_id, wait)] = approvals.as_slice() else {
        panic!("expected one saved approval")
    };
    assert_eq!(Some(*agent_id), worker_id);
    assert!(
        matches!(&wait.on, WaitOn::ToolApproval { tool_name, arguments }
        if tool_name == "prices.lookup" && *arguments == json!({"sku": "notebook"}))
    );
    assert!(
        !store
            .resolve_wait(&wait.id, WakeEvent::Webhook(json!({"approved": true})))
            .unwrap()
    );
    assert!(
        store
            .resolve_wait(&wait.id, WakeEvent::Approval { approved: true })
            .unwrap()
    );
    assert!(store.pending_tool_approvals().unwrap().is_empty());
    drop(store);

    let cancellation = CancellationToken::new();
    let store = WorkspaceStore::open(path).unwrap();
    let mut outcomes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        store.run_scheduler(
            NonZeroUsize::MIN,
            &cancellation,
            |run| {
                if run.delegation.is_some() {
                    ScheduledRunConfig::new(
                        &worker,
                        &worker_skills,
                        research_options(ToolGrant::RequireApproval),
                    )
                } else {
                    ScheduledRunConfig::new(
                        &Coordinator,
                        &coordinator_skills,
                        coordinator_options(),
                    )
                }
            },
            |run, outcome| {
                let RunOutcome::Finished(report) = outcome else {
                    panic!("expected the approved run to finish")
                };
                outcomes.push(report.answer.clone());
                if run.delegation.is_none() {
                    cancellation.cancel();
                }
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcomes, ["350 cents", "The notebook costs 350 cents."]);
    assert!(path.join("lookup").exists());
}
