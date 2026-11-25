use std::{fs, path::Path, process::Command};

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    JsonValue, Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, RunError,
    RunEvent, RunOptions, Skill, TokenUsage, Tool, ToolCall, ToolContext, ToolError, ToolSpec,
    WorkspaceStore,
};

const PROMPT: &str = "Write a note.";
const CHILD_STATE: &str = "WAKEFOLD_DURABLE_SKILL_TEST";
const SUMMARY: &str = "Notes loaded.";

struct WriteNote;

#[async_trait]
impl Tool for WriteNote {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("write", "Write the note", json!({"type": "object"}))
    }

    async fn call(&self, _: &JsonValue) -> Result<JsonValue, ToolError> {
        panic!("expected the run workspace")
    }

    async fn call_with_context(
        &self,
        ctx: ToolContext,
        _: &JsonValue,
    ) -> Result<JsonValue, ToolError> {
        let workspace = ctx.workspace().unwrap();
        let path = workspace
            .root()
            .join(workspace.agent_directory())
            .join("note.txt");
        fs::File::create_new(path).expect("a recovered run must never repeat this write");
        Ok(json!("written"))
    }
}

fn load(id: &str) -> ModelResponse {
    ModelResponse::ToolCall(ToolCall::new(id, "skills.load", json!({"name": "notes"})))
}

fn write_note() -> ModelResponse {
    ModelResponse::ToolCalls {
        text: Some("x".repeat(200)),
        calls: vec![ToolCall::new("write", "notes.write", json!({}))],
    }
}

struct BeforeRestart;

#[async_trait]
impl Provider for BeforeRestart {
    async fn complete(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver requests full model turns")
    }

    async fn complete_turn(
        &self,
        req: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let response = if req.tools.is_empty() {
            ModelResponse::Final(SUMMARY.into())
        } else if req.messages.len() == 1 {
            load("load")
        } else if matches!(req.messages.get(1), Some(Message::Summary(_))) {
            // The checkpoint and next request are already committed. Skip all destructors.
            std::process::exit(73);
        } else {
            write_note()
        };
        let mut turn = ModelTurn::new(response, Some(TokenUsage::new(2, 1)));
        turn.context = Some(json!("ctx"));
        Ok(turn)
    }
}

struct AfterRestart;

#[async_trait]
impl Provider for AfterRestart {
    async fn complete(&self, req: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        assert_eq!(
            req.tools
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["skills.load", "notes.write"]
        );
        assert_eq!(
            req.messages,
            &[
                Message::User(PROMPT.into()),
                Message::Summary(SUMMARY.into()),
                Message::Assistant(write_note()),
                Message::Tool(wakefold::ToolResult::new("write", json!("written"))),
            ]
        );
        assert_eq!(req.assistant_context, &[Some(json!("ctx"))]);
        Ok(ModelResponse::Final("done".into()))
    }
}

fn run_note(path: &Path, provider: &dyn Provider) -> Result<wakefold::RunReport, RunError> {
    let mut store = WorkspaceStore::open(path).unwrap();
    let mut journal = store.open_journal("chat", "notes").unwrap();
    block_on(journal.run(
        PROMPT,
        provider,
        &[Skill::new("notes", "Manage notes", &[&WriteNote])],
        RunOptions::new(3).with_context_budget(265),
    ))
}

#[test]
fn process_restart_restores_loaded_tools_after_compaction() {
    if let Some(path) = std::env::var_os(CHILD_STATE) {
        panic!(
            "expected process exit during next model request, got {:?}",
            run_note(Path::new(&path), &BeforeRestart)
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "process_restart_restores_loaded_tools_after_compaction",
            "--nocapture",
        ])
        .env(CHILD_STATE, dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let report = run_note(dir.path(), &AfterRestart).unwrap();
    assert_eq!(report.answer, "done");
    assert_eq!(report.usage.reported, TokenUsage::new(6, 3));
    assert_eq!(report.usage.unreported_turns, 1);
    assert_eq!(run_note(dir.path(), &NoCalls).unwrap(), report);

    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let entries = store
        .open_journal("chat", "notes")
        .unwrap()
        .load_after(0)
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| matches!(e.event, RunEvent::SkillLoaded(_)))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| matches!(e.event, RunEvent::Compacted(_)))
            .count(),
        1
    );
}

struct NoCalls;

#[async_trait]
impl Provider for NoCalls {
    async fn complete(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        panic!("unexpected provider call during recovery")
    }
}

struct LoadTwice;

#[async_trait]
impl Provider for LoadTwice {
    async fn complete(&self, req: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        match req.messages.len() {
            1 => Ok(load("first")),
            3 => Ok(load("again")),
            5 => {
                assert_eq!(
                    req.tools
                        .iter()
                        .map(|tool| tool.name.as_str())
                        .collect::<Vec<_>>(),
                    ["skills.load", "notes.write"]
                );
                Ok(ModelResponse::Final("done".into()))
            }
            _ => panic!("unexpected transcript {:?}", req.messages),
        }
    }
}

#[test]
fn failed_loader_commit_rolls_back_activation_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let db = rusqlite::Connection::open(dir.path().join("wakefold.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER interrupt_loader BEFORE INSERT ON run_events
        WHEN json_extract(NEW.event_json, '$.kind') = 'tool_call_completed'
        BEGIN SELECT RAISE(ABORT, 'interrupt loader'); END;",
    )
    .unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    let tools: [&dyn Tool; 1] = [&WriteNote];
    let skills = [Skill::new("notes", "Manage notes", &tools)];
    let result = block_on(journal.run(PROMPT, &LoadTwice, &skills, RunOptions::new(3)));
    assert!(matches!(result, Err(RunError::Journal(_))));
    let entries = journal.load_after(0).unwrap();
    assert!(matches!(
        entries.last().unwrap().event,
        RunEvent::ToolCallStarted(_)
    ));
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry.event, RunEvent::SkillLoaded(_)))
    );
    db.execute_batch("DROP TRIGGER interrupt_loader").unwrap();
    let report = block_on(journal.run(PROMPT, &LoadTwice, &skills, RunOptions::new(3))).unwrap();
    assert_eq!(report.answer, "done");
    let entries = journal.load_after(0).unwrap();
    let loads: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.event, RunEvent::SkillLoaded(_)))
        .collect();
    assert_eq!(loads.len(), 1);
    assert!(
        matches!(&entries[loads[0].sequence as usize].event, RunEvent::ToolCallCompleted(result) if result.call_id == "first".into())
    );
    assert!(matches!(
        block_on(journal.run("different", &NoCalls, &skills, RunOptions::new(3))),
        Err(RunError::InvalidJournal(_))
    ));
    assert!(matches!(
        block_on(journal.run(PROMPT, &NoCalls, &[], RunOptions::new(3))),
        Err(RunError::Tool { .. })
    ));
}

#[test]
fn interrupted_application_tool_is_never_dispatched_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    let loader = load("first");
    let call = write_note();
    journal
        .append(
            0,
            &[
                RunEvent::RunStarted(PROMPT.into()),
                RunEvent::ModelCallRequested,
                RunEvent::ModelCallCompleted(ModelTurn::new(loader.clone(), None)),
                RunEvent::ToolCallStarted(loader.tool_calls()[0].clone()),
                RunEvent::SkillLoaded("notes".into()),
                RunEvent::ToolCallCompleted(wakefold::ToolResult::new(
                    "first",
                    json!({"loaded": "notes"}),
                )),
                RunEvent::ModelCallRequested,
                RunEvent::ModelCallCompleted(ModelTurn::new(call.clone(), None)),
                RunEvent::ToolCallStarted(call.tool_calls()[0].clone()),
            ],
        )
        .unwrap();
    let result = block_on(journal.run(
        PROMPT,
        &NoCalls,
        &[Skill::new("notes", "Manage notes", &[&WriteNote])],
        RunOptions::new(3),
    ));
    assert!(
        matches!(result, Err(RunError::ToolOutcomeUnknown { name, id }) if name == "notes.write" && id == "write".into())
    );
}

#[test]
fn resumes_a_saved_tool_request_from_an_earlier_journal_without_a_checkpoint() {
    // Existing journals may have only the loader's successful ToolCallCompleted.
    let dir = tempfile::tempdir().unwrap();
    let loader = load("first");
    {
        let mut store = WorkspaceStore::open(dir.path()).unwrap();
        store
            .open_journal("chat", "old")
            .unwrap()
            .append(
                0,
                &[
                    RunEvent::RunStarted(PROMPT.into()),
                    RunEvent::ModelCallRequested,
                    RunEvent::ModelCallCompleted(ModelTurn::new(loader.clone(), None)),
                    RunEvent::ToolCallStarted(loader.tool_calls()[0].clone()),
                    RunEvent::ToolCallCompleted(wakefold::ToolResult::new(
                        "first",
                        json!({"loaded": "notes"}),
                    )),
                    RunEvent::ModelCallRequested,
                    RunEvent::ModelCallCompleted(ModelTurn::new(write_note(), None)),
                ],
            )
            .unwrap();
    }
    struct Finish;
    #[async_trait]
    impl Provider for Finish {
        async fn complete(&self, req: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
            assert!(
                matches!(req.messages.last(), Some(Message::Tool(result)) if result.value == "written")
            );
            assert_eq!(
                req.tools
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>(),
                ["skills.load", "notes.write"]
            );
            Ok(ModelResponse::Final("saved".into()))
        }
    }
    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let mut journal = store.open_journal("chat", "old").unwrap();
    let report = block_on(journal.run(
        PROMPT,
        &Finish,
        &[Skill::new("notes", "Manage notes", &[&WriteNote])],
        RunOptions::new(3),
    ))
    .unwrap();
    assert_eq!(report.answer, "saved");
}
