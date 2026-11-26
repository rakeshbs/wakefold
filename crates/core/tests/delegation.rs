use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    CancellationToken, DelegateTool, Message, ModelRequest, ModelResponse, Provider, ProviderError,
    RunEvent, RunOptions, RunOutcome, ScheduledRunConfig, Skill, Tool, ToolCall, WaitOn,
    WorkspaceStore,
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
