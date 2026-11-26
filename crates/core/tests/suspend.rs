use std::num::NonZeroUsize;
use std::time::Duration;

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    JsonValue, Message, ModelRequest, ModelResponse, PendingWait, Provider, ProviderError,
    RunError, RunEvent, RunOptions, RunOutcome, Skill, TimerWait, Tool, ToolCall, ToolContext,
    ToolError, ToolOutput, ToolSpec, WaitOn, WakeEvent, WorkspaceStore,
};

const PROMPT: &str = "Wait and report the result.";
const ONE: NonZeroUsize = NonZeroUsize::MIN;

struct Workflow {
    tool: &'static str,
    result: Option<JsonValue>,
}

#[async_trait]
impl Provider for Workflow {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        Ok(match request.messages.last().unwrap() {
            Message::User(_) => {
                assert!(self.result.is_none(), "the saved model call must be reused");
                ToolCall::new("load", "load_skill", json!({"name": "test"})).into()
            }
            Message::Tool(result) if result.call_id == "load".into() => {
                assert!(self.result.is_none(), "the saved model call must be reused");
                ToolCall::new(
                    "suspend",
                    "call_tool",
                    json!({"tool_name": self.tool, "arguments": {"seconds": 10}}),
                )
                .into()
            }
            Message::Tool(result) => {
                assert_eq!(result.call_id, "suspend".into());
                assert_eq!(Some(&result.value), self.result.as_ref());
                ModelResponse::Final("done".into())
            }
            message => panic!("unexpected message {message:?}"),
        })
    }
}

struct NoModel;

#[async_trait]
impl Provider for NoModel {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        panic!("a pending wait should yield immediately")
    }
}

fn run(
    store: &mut WorkspaceStore,
    provider: &dyn Provider,
    tool: &dyn Tool,
) -> Result<RunOutcome, RunError> {
    let mut journal = store.open_journal("chat", "run").unwrap();
    block_on(journal.run_until_blocked(
        PROMPT,
        provider,
        &[Skill::new("test", "Test waits", &[tool])],
        RunOptions::new(4),
    ))
}

fn suspended(outcome: RunOutcome) -> PendingWait {
    let RunOutcome::Suspended(wait) = outcome else {
        panic!("expected a suspended run, received {outcome:?}");
    };
    wait
}

#[test]
fn timer_and_wake_survive_reopening_the_database() {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(root.path()).unwrap();
    let wait = suspended(
        run(
            &mut store,
            &Workflow {
                tool: "test.wait",
                result: None,
            },
            &TimerWait,
        )
        .unwrap(),
    );
    let WaitOn::Timer { wake_at } = wait.on else {
        panic!("expected a timer wait");
    };
    drop(store);

    let mut store = WorkspaceStore::open(root.path()).unwrap();
    assert_eq!(
        run(&mut store, &NoModel, &TimerWait).unwrap(),
        RunOutcome::Suspended(wait)
    );
    assert_eq!(
        store
            .wake_due_timers(wake_at - Duration::from_millis(1), ONE)
            .unwrap(),
        0
    );
    let after_deadline = wake_at + Duration::from_millis(1);
    let db = rusqlite::Connection::open(root.path().join("wakefold.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER interrupt_wake BEFORE INSERT ON run_events
         WHEN json_extract(NEW.event_json, '$.kind') = 'woken'
         BEGIN SELECT RAISE(ABORT, 'interrupt wake'); END;",
    )
    .unwrap();
    assert!(store.wake_due_timers(after_deadline, ONE).is_err());
    assert!(matches!(
        run(&mut store, &NoModel, &TimerWait).unwrap(),
        RunOutcome::Suspended(_)
    ));
    db.execute_batch("DROP TRIGGER interrupt_wake").unwrap();
    assert_eq!(store.wake_due_timers(after_deadline, ONE).unwrap(), 1);
    assert_eq!(store.wake_due_timers(after_deadline, ONE).unwrap(), 0);
    let events = store
        .open_journal("chat", "run")
        .unwrap()
        .load_after(0)
        .unwrap();
    let RunEvent::Woken { payload, .. } = &events.last().unwrap().event else {
        panic!("expected a persisted wake event");
    };
    assert!(payload["wake_at_unix_ms"].is_i64());
    let resumed = Workflow {
        tool: "test.wait",
        result: Some(payload.clone()),
    };
    drop(store);
    let mut store = WorkspaceStore::open(root.path()).unwrap();
    assert!(matches!(run(&mut store, &resumed, &TimerWait).unwrap(),
        RunOutcome::Finished(report) if report.answer == "done"));
}

struct ExternalWait(WaitOn);

#[async_trait]
impl Tool for ExternalWait {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "wait",
            "Wait for an external event",
            json!({ "type": "object" }),
        )
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        panic!("expected the durable driver");
    }

    async fn call_with_output(
        &self,
        _: ToolContext,
        _: &JsonValue,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::Wait(self.0.clone()))
    }
}

#[test]
fn external_events_only_resolve_matching_waits_once() {
    for (on, event, payload) in [
        (
            WaitOn::Message,
            WakeEvent::Message(json!("worker result")),
            json!("worker result"),
        ),
        (
            WaitOn::Webhook,
            WakeEvent::Webhook(json!({"status": "done"})),
            json!({"status": "done"}),
        ),
        (
            WaitOn::Approval,
            WakeEvent::Approval { approved: false },
            json!({"approved": false}),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::open(root.path()).unwrap();
        let tool = ExternalWait(on);
        let wait = suspended(
            run(
                &mut store,
                &Workflow {
                    tool: "test.wait",
                    result: None,
                },
                &tool,
            )
            .unwrap(),
        );
        let wrong = if wait.on == WaitOn::Message {
            WakeEvent::Webhook(json!(null))
        } else {
            WakeEvent::Message(json!(null))
        };
        assert!(!store.resolve_wait(&wait.id, wrong).unwrap());
        assert!(!store.resolve_wait("other-wait", event.clone()).unwrap());
        assert!(store.resolve_wait(&wait.id, event.clone()).unwrap());
        assert!(!store.resolve_wait(&wait.id, event).unwrap());
        let outcome = run(
            &mut store,
            &Workflow {
                tool: "test.wait",
                result: Some(payload),
            },
            &tool,
        )
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Finished(_)));
    }
}

#[test]
fn a_wait_requires_a_durable_run() {
    let result = block_on(wakefold::run(
        PROMPT,
        &Workflow {
            tool: "test.wait",
            result: None,
        },
        &[Skill::new("test", "Test waits", &[&TimerWait])],
        4,
    ));
    assert!(matches!(result, Err(RunError::WaitRequiresJournal { name }) if name == "test.wait"));
}
