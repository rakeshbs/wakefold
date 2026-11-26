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
    Effect, JsonValue, Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError,
    RunError, RunEvent, RunOptions, RunReport, Skill, TokenUsage, Tool, ToolCall, ToolContext,
    ToolError, ToolSpec, WorkspaceStore,
};

const WORKER_ROOT: &str = "WAKEFOLD_RECOVERY_WORKER_ROOT";
const FAULT_POINT: &str = "WAKEFOLD_RECOVERY_FAULT_POINT";
const POLICIES: [Effect; 3] = [Effect::ReadOnly, Effect::Idempotent, Effect::NonIdempotent];
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
    effect: Effect,
}

#[async_trait]
impl Tool for Operation<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            self.name,
            "Execute this workflow operation",
            json!({"type": "object"}),
        )
        .with_effect(self.effect)
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
            .idempotency_key()
            .expect("agent calls must have an idempotency key");
        append_line(self.root.join(format!("{}-attempts", self.name)), key);
        let result = match self.effect {
            Effect::ReadOnly => json!(fs::read_to_string(self.root.join("catalog")).unwrap()),
            Effect::Idempotent => {
                // The backing service commits the key and side effect together.
                let mut service = Connection::open(self.root.join("reservations.sqlite3")).unwrap();
                service
                    .execute_batch(
                        "CREATE TABLE IF NOT EXISTS reservations (
                    id INTEGER PRIMARY KEY, idempotency_key TEXT NOT NULL UNIQUE,
                    arguments TEXT NOT NULL)",
                    )
                    .unwrap();
                let tx = service
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .unwrap();
                tx.execute(
                    "INSERT INTO reservations(idempotency_key, arguments)
                    VALUES (?1, ?2) ON CONFLICT(idempotency_key) DO NOTHING",
                    params![key, arguments.to_string()],
                )
                .unwrap();
                let (id, original): (i64, String) = tx
                    .query_row(
                        "SELECT id, arguments FROM reservations WHERE idempotency_key = ?1",
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
            Effect::NonIdempotent => {
                fs::File::create_new(self.root.join("notification"))
                    .expect("a notification must never repeat");
                json!("sent")
            }
            _ => unreachable!("the test uses the three declared policies"),
        };
        pause_at(&format!("{}-effect", self.name));
        Ok(result)
    }
}

struct Workflow<'a>(&'a Path);

#[cfg(feature = "derive")]
#[derive(serde::Deserialize, schemars::JsonSchema, wakefold::WakefoldTool)]
#[serde(deny_unknown_fields)]
#[tool(
    name = "reserve",
    description = "Reserve an item",
    effect = "idempotent"
)]
struct ReservationInput {
    quantity: u64,
}

#[async_trait]
impl Provider for Workflow<'_> {
    async fn complete(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver requests complete turns")
    }

    async fn complete_turn(
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
                    name, "call_tool", json!({"name": format!("workflow.{name}"), "arguments": {"quantity": 1}}))).collect(),
            },
            Message::Tool(result) if result.value == "sent" => ModelResponse::Final("reserved and notified".into()),
            other => panic!("unexpected model boundary {other:?}"),
        };
        if matches!(&response, ModelResponse::ToolCalls { .. }) {
            pause_at("model-response");
        }
        let mut turn = ModelTurn::new(response, Some(TokenUsage::new(1, 1)));
        turn.context = Some(json!({"saved": request.messages.len()}));
        Ok(turn)
    }
}

fn run_workflow(
    root: &Path,
    policies: [Effect; 3],
    hook: Option<fn(u64, &[RunEvent])>,
) -> Result<RunReport, RunError> {
    let read = Operation {
        root,
        name: "read",
        effect: policies[0],
    };
    let reserve_operation = Operation {
        root,
        name: "reserve",
        effect: policies[1],
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
        effect: policies[2],
    };
    let tools: [&dyn Tool; 3] = [&read, &reserve, &notify];
    let skills = [Skill::new("workflow", "Read, reserve and notify", &tools)];
    let mut store = WorkspaceStore::open(root).unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    if let Some(hook) = hook {
        journal.set_commit_hook(hook);
    }
    block_on(journal.run(PROMPT, &Workflow(root), &skills, RunOptions::new(3)))
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
        let interrupted_notification = matches!(before.last(), Some(RunEvent::ToolCallStartedWithRecovery { call, .. }) if call.id == "notify".into());
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
            assert_eq!(report.usage.reported, TokenUsage::new(3, 3), "{point}");
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
                RunEvent::ToolCallStartedWithRecovery {
                    call,
                    idempotency_key,
                    ..
                } => Some((call.id.to_string(), idempotency_key.as_str())),
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
        ("read", Effect::NonIdempotent),
        ("notify", Effect::Idempotent),
    ] {
        let end = history.iter().position(|event| matches!(event, RunEvent::ToolCallStartedWithRecovery { call, .. } if call.id == name.into())).unwrap();
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
fn older_tool_starts_keep_the_unknown_outcome_default() {
    let baseline = tempfile::tempdir().unwrap();
    fs::write(baseline.path().join("catalog"), "notebook").unwrap();
    run_workflow(baseline.path(), POLICIES, None).unwrap();
    let mut history = events(baseline.path());
    let end = history.iter().position(|event| matches!(event, RunEvent::ToolCallStartedWithRecovery { call, .. } if call.id == "read".into())).unwrap();
    let RunEvent::ToolCallStartedWithRecovery { call, .. } = history[end].clone() else {
        unreachable!()
    };
    history[end] = RunEvent::ToolCallStarted(call);
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(root.path()).unwrap();
    store
        .open_journal("chat", "run")
        .unwrap()
        .append(0, &history[..=end])
        .unwrap();
    assert!(matches!(
        run_workflow(root.path(), POLICIES, None),
        Err(RunError::ToolOutcomeUnknown { .. })
    ));
    assert!(lines(root.path(), "read-attempts").is_empty());
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
fn old_tool_specs_default_to_non_idempotent_and_round_trip_effects() {
    let old = json!({"name": "write", "description": "Write", "parameters": {"type": "object"}});
    let spec: ToolSpec = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(spec.effect, Effect::NonIdempotent);
    assert_eq!(serde_json::to_value(&spec).unwrap(), old);
    for effect in POLICIES {
        let spec = spec.clone().with_effect(effect);
        let restored: ToolSpec =
            serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(restored, spec);
    }
    assert_eq!(ToolContext::default().idempotency_key(), None);
}
