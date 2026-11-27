use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    CancellationToken, JsonValue, Message, ModelRequest, ModelResponse, PendingWait, Provider,
    ProviderError, RunError, RunEvent, RunOptions, RunOutcome, ScheduledRunConfig, Skill,
    TimerWait, Tool, ToolCall, ToolContext, ToolError, ToolOutput, ToolSpec, WaitOn, WakeEvent,
    WorkspaceStore,
};

const CHAT: wakefold::ChatId = wakefold::ChatId::from_u128(1);

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
    let mut journal = store.open_journal(CHAT, "run").unwrap();
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
        .open_journal(CHAT, "run")
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

fn start_timer(store: &mut WorkspaceStore, key: &str, wake_at: SystemTime) {
    let provider = Workflow {
        tool: "test.wait",
        result: None,
    };
    let tool = ExternalWait(WaitOn::Timer { wake_at });
    let tools: [&dyn Tool; 1] = [&tool];
    let skill = Skill::new("test", "Test waits", &tools);
    let mut journal = store.open_journal(CHAT, key).unwrap();
    let outcome =
        block_on(journal.run_until_blocked(PROMPT, &provider, &[skill], RunOptions::new(4)))
            .unwrap();
    assert_eq!(suspended(outcome).on, WaitOn::Timer { wake_at });
}

fn timer_result(wake_at: SystemTime) -> JsonValue {
    let ms = wake_at
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .div_ceil(1_000_000);
    json!({"wake_at_unix_ms": ms})
}

struct TimedWork {
    provider: Workflow,
    active: AtomicUsize,
    peak: AtomicUsize,
}

#[async_trait]
impl Provider for TimedWork {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        // Keep this turn pending so a second run can enter its model request.
        futures_timer::Delay::new(Duration::from_millis(50)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.provider.complete_response(request).await
    }
}

#[test]
fn scheduler_wakes_timers_with_bounded_concurrency() {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(root.path()).unwrap();
    let wake_at = SystemTime::now() + Duration::from_millis(300);
    const RUNS: usize = 5;
    for run in 0..RUNS {
        start_timer(&mut store, &run.to_string(), wake_at);
    }
    let provider = TimedWork {
        provider: Workflow {
            tool: "test.wait",
            result: Some(timer_result(wake_at)),
        },
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let tool = ExternalWait(WaitOn::Timer { wake_at });
    let tools: [&dyn Tool; 1] = [&tool];
    let skills = [Skill::new("test", "Test waits", &tools)];
    let cancellation = CancellationToken::new();
    let mut finished = std::collections::HashSet::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(10),
            store.run_scheduler(
                NonZeroUsize::new(2).unwrap(),
                &cancellation,
                |_| ScheduledRunConfig::new(&provider, &skills, RunOptions::new(4)),
                |run, outcome| {
                    assert!(SystemTime::now() >= wake_at);
                    assert!(matches!(outcome, RunOutcome::Finished(_)));
                    assert!(finished.insert(run.agent_id), "the run must finish once");
                    if finished.len() == RUNS {
                        cancellation.cancel();
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
    });
    assert_eq!(finished.len(), RUNS);
    assert_eq!(provider.peak.load(Ordering::SeqCst), 2);
}

#[test]
fn scheduler_retries_a_resolved_wait_after_a_driver_error() {
    let root = tempfile::tempdir().unwrap();
    let store_path = root.path();
    let mut store = WorkspaceStore::open(store_path).unwrap();
    let tool = ExternalWait(WaitOn::Webhook);
    let waiting = Workflow {
        tool: "test.wait",
        result: None,
    };
    let wait = suspended(run(&mut store, &waiting, &tool).unwrap());
    let payload = json!({"status": "done"});
    assert!(
        store
            .resolve_wait(&wait.id, WakeEvent::Webhook(payload.clone()))
            .unwrap()
    );

    // Missing configuration stops this scheduler. The ready run must stay on disk.
    let cancellation = CancellationToken::new();
    let failed = block_on(store.run_scheduler(
        ONE,
        &cancellation,
        |_| ScheduledRunConfig::new(&NoModel, &[], RunOptions::new(4)),
        |_, _| panic!("the first scheduler omitted the previously loaded skill"),
    ));
    assert!(
        matches!(failed, Err(wakefold::SchedulerError::Resume { source, .. })
        if matches!(source.as_ref(), RunError::MissingSkillOnResume { name } if name == "test"))
    );

    let resumed = Workflow {
        tool: "test.wait",
        result: Some(payload),
    };
    let tools: [&dyn Tool; 1] = [&tool];
    let skills = [Skill::new("test", "Test waits", &tools)];
    let mut finished = false;
    block_on(WorkspaceStore::open(store_path).unwrap().run_scheduler(
        ONE,
        &cancellation,
        |_| ScheduledRunConfig::new(&resumed, &skills, RunOptions::new(4)),
        |_, outcome| {
            assert!(matches!(outcome, RunOutcome::Finished(_)));
            finished = true;
            cancellation.cancel();
        },
    ))
    .unwrap();
    assert!(finished);
}

const SCALE_STATE: &str = "WAKEFOLD_TIMER_SCALE_STATE";
const RUNS: usize = 1_000;

fn resident_kib() -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

#[test]
#[ignore = "scale benchmark starts a child process and waits for timers to become due"]
fn a_thousand_timers_survive_restart() {
    if let Some(path) = std::env::var_os(SCALE_STATE) {
        let path = Path::new(&path);
        let mut store = WorkspaceStore::open(path).unwrap();
        let before = resident_kib();
        let wake_at = SystemTime::now() + Duration::from_secs(8);
        let started = Instant::now();
        for run in 0..RUNS {
            start_timer(&mut store, &run.to_string(), wake_at);
        }
        let registration_time = started.elapsed();
        std::fs::write(path.join("deadline"), timer_result(wake_at).to_string()).unwrap();
        let cancellation = CancellationToken::new();
        block_on(async {
            let sample = async {
                futures_timer::Delay::new(Duration::from_millis(300)).await;
                assert!(
                    SystemTime::now() < wake_at,
                    "timers should be pending at sample time"
                );
                eprintln!(
                    "timer benchmark: registered {RUNS} in {registration_time:?}, RSS before {before:?} KiB, idle RSS {:?} KiB",
                    resident_kib()
                );
                cancellation.cancel();
            };
            let scheduler = store.run_scheduler(
                ONE,
                &cancellation,
                |_| panic!("future timers must not be active"),
                |_, _| unreachable!(),
            );
            let ((), result) = futures_util::future::join(sample, scheduler).await;
            result.unwrap();
        });
        // Leave all waits pending and simulate exiting without closing application state.
        std::process::exit(73);
    }

    let root = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "a_thousand_timers_survive_restart",
            "--nocapture",
        ])
        .env(SCALE_STATE, root.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let result: JsonValue =
        serde_json::from_str(&std::fs::read_to_string(root.path().join("deadline")).unwrap())
            .unwrap();
    let wake_at = UNIX_EPOCH + Duration::from_millis(result["wake_at_unix_ms"].as_u64().unwrap());
    std::thread::sleep(
        wake_at
            .duration_since(SystemTime::now())
            .unwrap_or_default(),
    );

    let provider = Workflow {
        tool: "test.wait",
        result: Some(result),
    };
    let tool = ExternalWait(WaitOn::Timer { wake_at });
    let tools: [&dyn Tool; 1] = [&tool];
    let skills = [Skill::new("test", "Test waits", &tools)];
    let store = WorkspaceStore::open(root.path()).unwrap();
    let cancellation = CancellationToken::new();
    let started = Instant::now();
    let mut finished = std::collections::HashSet::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(30),
            store.run_scheduler(
                NonZeroUsize::new(16).unwrap(),
                &cancellation,
                |_| ScheduledRunConfig::new(&provider, &skills, RunOptions::new(4)),
                |run, outcome| {
                    assert!(
                        matches!(outcome, RunOutcome::Finished(report) if report.answer == "done")
                    );
                    assert!(finished.insert(run.agent_id), "the run must finish once");
                    if finished.len() == RUNS {
                        cancellation.cancel();
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
    });
    assert_eq!(finished.len(), RUNS);
    eprintln!(
        "timer benchmark: resumed all {RUNS} overdue runs in {:?}",
        started.elapsed()
    );
}
