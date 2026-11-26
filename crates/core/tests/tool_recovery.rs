use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_executor::block_on;
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::json;
use wakefold::{
    JsonValue, Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, RunError,
    RunEvent, RunOptions, RunReport, Skill, TokenUsage, Tool, ToolCall, ToolContext, ToolError,
    ToolRecovery, ToolSpec, WorkspaceStore,
};

const WORKER_ROOT: &str = "WAKEFOLD_RECOVERY_WORKER_ROOT";
const FAULT_POINT: &str = "WAKEFOLD_RECOVERY_FAULT_POINT";
const POLICIES: [ToolRecovery; 3] = [
    ToolRecovery::RetryAfterCrash,
    ToolRecovery::RetryAfterCrash,
    ToolRecovery::StopAfterCrash,
];
const PROMPT: &str = "Read the catalog, reserve an item, and send one notification.";

fn append_line(path: PathBuf, value: &str) {
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap(),
        "{value}"
    )
    .unwrap();
}

fn pause_at(point: &str) {
    if std::env::var(FAULT_POINT).as_deref() != Ok(point) {
        return;
    }
    let root = PathBuf::from(std::env::var_os(WORKER_ROOT).unwrap());
    fs::write(root.join("waiting"), point).unwrap();
    // Give the parent a bounded window to kill this worker at the requested boundary.
    std::thread::sleep(Duration::from_secs(15));
    panic!("parent did not terminate the worker at {point}");
}

fn pause_after_commit(sequence: u64, _: &[RunEvent]) {
    pause_at(&format!("commit-{sequence}"));
}

struct Operation<'a> {
    root: &'a Path,
    name: &'static str,
    recovery: ToolRecovery,
}

#[async_trait]
impl Tool for Operation<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.name,
            "Execute this workflow operation",
            json!({"type": "object"}),
        )
        .with_recovery(self.recovery)
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new("expected a context with an idempotency key"))
    }

    async fn call_with_context(
        &self,
        context: ToolContext,
        arguments: &JsonValue,
    ) -> Result<JsonValue, ToolError> {
        let key = context
            .tool_call_key()
            .expect("agent calls must have an idempotency key");
        append_line(self.root.join(format!("{}-attempts", self.name)), key);
        let result = match self.name {
            "read" => json!(fs::read_to_string(self.root.join("catalog")).unwrap()),
            "reserve" => {
                // The backing service commits the key and side effect together.
                let mut service = Connection::open(self.root.join("reservations.sqlite3")).unwrap();
                service
                    .execute_batch(
                        "CREATE TABLE IF NOT EXISTS reservations (
                    id INTEGER PRIMARY KEY, tool_call_key TEXT NOT NULL UNIQUE,
                    arguments TEXT NOT NULL)",
                    )
                    .unwrap();
                let tx = service
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .unwrap();
                tx.execute(
                    "INSERT INTO reservations(tool_call_key, arguments)
                    VALUES (?1, ?2) ON CONFLICT(tool_call_key) DO NOTHING",
                    params![key, arguments.to_string()],
                )
                .unwrap();
                let (id, original): (i64, String) = tx
                    .query_row(
                        "SELECT id, arguments FROM reservations WHERE tool_call_key = ?1",
                        [key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(
                    original,
                    arguments.to_string(),
                    "a key must identify the same request"
                );
                tx.commit().unwrap();
                json!({"reservation": format!("R-{id}")})
            }
            "notify" => {
                fs::File::create_new(self.root.join("notification"))
                    .expect("a notification must never repeat");
                json!("sent")
            }
            _ => unreachable!("the test uses three known operations"),
        };
        pause_at(&format!("{}-effect", self.name));
        Ok(result)
    }
}

struct Workflow<'a>(&'a Path);

#[cfg(feature = "derive")]
#[derive(serde::Deserialize, schemars::JsonSchema, wakefold::ToolInput)]
#[serde(deny_unknown_fields)]
#[tool(
    name = "reserve",
    description = "Reserve an item",
    recovery = "retry_after_crash"
)]
struct ReservationInput {
    quantity: u64,
}

#[async_trait]
impl Provider for Workflow<'_> {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver requests complete turns")
    }

    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        assert_eq!(
            request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["load_skill", "call_tool"]
        );
        append_line(self.0.join("model-calls"), "called");
        let response = match request.messages.last().unwrap() {
            Message::User(_) => ModelResponse::ToolCall(ToolCall::new("load", "load_skill", json!({"name": "workflow"}))),
            Message::Tool(result) if result.value["loaded"] == "workflow" => ModelResponse::ToolCalls {
                text: None,
                calls: ["read", "reserve", "notify"].into_iter().map(|name| ToolCall::new(
                    name, "call_tool", json!({"tool_name": format!("workflow.{name}"), "arguments": {"quantity": 1}}))).collect(),
            },
            Message::Tool(result) if result.value == "sent" => ModelResponse::Final("reserved and notified".into()),
            other => panic!("unexpected model boundary {other:?}"),
        };
        if matches!(&response, ModelResponse::ToolCalls { .. }) {
            pause_at("model-response");
        }
        let mut turn = ModelTurn::new(response, Some(TokenUsage::new(1, 1)));
        turn.replay_data = Some(json!({"saved": request.messages.len()}));
        Ok(turn)
    }
}

fn run_workflow(
    root: &Path,
    policies: [ToolRecovery; 3],
    hook: Option<fn(u64, &[RunEvent])>,
) -> Result<RunReport, RunError> {
    let read = Operation {
        root,
        name: "read",
        recovery: policies[0],
    };
    let reserve_operation = Operation {
        root,
        name: "reserve",
        recovery: policies[1],
    };
    #[cfg(feature = "derive")]
    let reserve = wakefold::typed_tool_with_context(|input: ReservationInput, context| {
        let operation = &reserve_operation;
        async move {
            operation
                .call_with_context(context, &json!({"quantity": input.quantity}))
                .await
        }
    });
    #[cfg(not(feature = "derive"))]
    let reserve = reserve_operation;
    let notify = Operation {
        root,
        name: "notify",
        recovery: policies[2],
    };
    let tools: [&dyn Tool; 3] = [&read, &reserve, &notify];
    let skills = [Skill::new("workflow", "Read, reserve and notify", &tools)];
    let mut store = WorkspaceStore::open(root).unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    if let Some(hook) = hook {
        journal.set_commit_hook(hook);
    }
    block_on(journal.run_or_resume(PROMPT, &Workflow(root), &skills, RunOptions::new(3)))
}

fn events(root: &Path) -> Vec<RunEvent> {
    let mut store = WorkspaceStore::open(root).unwrap();
    store
        .open_journal("chat", "run")
        .unwrap()
        .load_after(0)
        .unwrap()
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

fn lines(root: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(root.join(name))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn kill_at(root: &Path, point: &str) {
    let mut worker = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "process_crashes_at_every_durable_boundary",
            "--nocapture",
        ])
        .env(WORKER_ROOT, root)
        .env(FAULT_POINT, point)
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !root.join("waiting").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "worker never reached {point}"
        );
        assert!(
            worker.try_wait().unwrap().is_none(),
            "worker exited before {point}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::read_to_string(root.join("waiting")).unwrap(), point);
    worker.kill().unwrap();
    worker.wait().unwrap();
}

#[test]
fn process_crashes_at_every_durable_boundary() {
    if let Some(root) = std::env::var_os(WORKER_ROOT) {
        run_workflow(Path::new(&root), POLICIES, Some(pause_after_commit)).unwrap();
        panic!("worker finished before its requested crash point");
    }
    let baseline = tempfile::tempdir().unwrap();
    fs::write(baseline.path().join("catalog"), "notebook").unwrap();
    run_workflow(baseline.path(), POLICIES, None).unwrap();
    let baseline_events = events(baseline.path());
    let mut points = baseline_events
        .iter()
        .enumerate()
        .filter(|(_, event)| !matches!(event, RunEvent::SkillLoaded(_)))
        .map(|(index, _)| format!("commit-{}", index + 1))
        .collect::<Vec<_>>();
    points.extend(
        [
            "model-response",
            "read-effect",
            "reserve-effect",
            "notify-effect",
        ]
        .map(str::to_owned),
    );

    for point in points {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("catalog"), "notebook").unwrap();
        kill_at(root.path(), &point);
        let before = events(root.path());
        let notification_attempts = lines(root.path(), "notify-attempts");
        let interrupted_notification = matches!(before.last(), Some(RunEvent::ToolCallPrepared { call, .. }) if call.id == "notify".into());
        let result = run_workflow(root.path(), POLICIES, None);
        if interrupted_notification {
            assert!(
                matches!(result, Err(RunError::ToolOutcomeUnknown { name, .. }) if name == "workflow.notify"),
                "{point}"
            );
            assert_eq!(
                lines(root.path(), "notify-attempts"),
                notification_attempts,
                "{point}"
            );
        } else {
            let report = result.unwrap_or_else(|error| panic!("{point}: {error}"));
            assert_eq!(report.answer, "reserved and notified", "{point}");
            assert_eq!(
                report.usage.reported_tokens,
                TokenUsage::new(3, 3),
                "{point}"
            );
            assert_eq!(
                lines(root.path(), "model-calls").len(),
                if point == "model-response" { 4 } else { 3 },
                "completed model calls must be reused at {point}"
            );
        }
        let service = Connection::open(root.path().join("reservations.sqlite3")).unwrap();
        let reservations: i64 = service
            .query_row("SELECT COUNT(*) FROM reservations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            reservations, 1,
            "{point} must create exactly one reservation"
        );
        let saved = events(root.path());
        let starts: Vec<_> = saved
            .iter()
            .filter_map(|event| match event {
                RunEvent::ToolCallPrepared {
                    call,
                    tool_call_key,
                    ..
                } => Some((call.id.to_string(), tool_call_key.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts
                .iter()
                .map(|(_, key)| *key)
                .collect::<HashSet<_>>()
                .len(),
            starts.len(),
            "each call needs a distinct key at {point}"
        );
        for name in ["read", "reserve", "notify"] {
            let key = starts.iter().find(|(id, _)| id == name).unwrap().1;
            assert!(
                lines(root.path(), &format!("{name}-attempts"))
                    .iter()
                    .all(|attempt| attempt == key),
                "retries must reuse the durable key at {point}"
            );
        }
        if point == "reserve-effect" {
            assert_eq!(lines(root.path(), "reserve-attempts").len(), 2);
        }
        if point == "read-effect" {
            assert_eq!(lines(root.path(), "read-attempts").len(), 2);
        }
    }
}

#[test]
fn recovery_requires_the_original_and_current_policies_to_allow_repetition() {
    let baseline = tempfile::tempdir().unwrap();
    fs::write(baseline.path().join("catalog"), "notebook").unwrap();
    run_workflow(baseline.path(), POLICIES, None).unwrap();
    let history = events(baseline.path());
    for (name, replacement) in [
        ("read", ToolRecovery::StopAfterCrash),
        ("notify", ToolRecovery::RetryAfterCrash),
    ] {
        let end = history.iter().position(|event| matches!(event, RunEvent::ToolCallPrepared { call, .. } if call.id == name.into())).unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut store = WorkspaceStore::open(root.path()).unwrap();
        store
            .open_journal("chat", "run")
            .unwrap()
            .append(0, &history[..=end])
            .unwrap();
        let mut policies = POLICIES;
        policies[if name == "read" { 0 } else { 2 }] = replacement;
        assert!(matches!(
            run_workflow(root.path(), policies, None),
            Err(RunError::ToolOutcomeUnknown { .. })
        ));
        assert!(lines(root.path(), &format!("{name}-attempts")).is_empty());
    }
}

#[test]
fn identical_call_ids_in_separate_runs_have_distinct_keys() {
    let mut keys = HashSet::new();
    for _ in 0..2 {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("catalog"), "notebook").unwrap();
        run_workflow(root.path(), POLICIES, None).unwrap();
        assert!(keys.insert(lines(root.path(), "reserve-attempts")[0].clone()));
    }
}

#[test]
fn tool_specs_default_to_stop_after_crash_and_round_trip_recovery() {
    let default_spec =
        json!({"name": "write", "description": "Write", "parameters": {"type": "object"}});
    let spec: ToolSpec = serde_json::from_value(default_spec.clone()).unwrap();
    assert_eq!(spec.recovery, ToolRecovery::StopAfterCrash);
    assert_eq!(serde_json::to_value(&spec).unwrap(), default_spec);
    for recovery in [ToolRecovery::RetryAfterCrash, ToolRecovery::StopAfterCrash] {
        let spec = spec.clone().with_recovery(recovery);
        let restored: ToolSpec =
            serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(restored, spec);
    }
    assert_eq!(ToolContext::default().tool_call_key(), None);
}
