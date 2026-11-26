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

struct ReadNote;

#[async_trait]
impl Tool for ReadNote {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("read", "Read the note", json!({"type": "object"}))
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
        fs::read(path).unwrap();
        Ok(json!("read"))
    }
}

fn definitions() -> JsonValue {
    let mut specs = vec![WriteNote.spec(), ReadNote.spec()];
    for spec in &mut specs {
        spec.name = format!("notes.{}", spec.name);
    }
    json!({"loaded": "notes", "tools": specs})
}

fn load(id: &str) -> ModelResponse {
    ModelResponse::ToolCall(ToolCall::new(id, "load_skill", json!({"name": "notes"})))
}

fn write_note() -> ModelResponse {
    ModelResponse::ToolCalls {
        text: Some("x".repeat(200)),
        calls: vec![ToolCall::new(
            "write",
            "call_tool",
            json!({"name": "notes.write", "arguments": {}}),
        )],
    }
}

fn read_note(id: &str) -> ModelResponse {
    ModelResponse::ToolCalls {
        text: Some("y".repeat(500)),
        calls: vec![ToolCall::new(
            id,
            "call_tool",
            json!({"name": "notes.read", "arguments": {}}),
        )],
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
        } else if req
            .messages
            .iter()
            .any(|message| matches!(message, Message::Summary(_)))
        {
            // The checkpoint and next request are already committed. Skip all destructors.
            std::process::exit(73);
        } else if matches!(req.messages.last(), Some(Message::Tool(result)) if result.value == "written")
        {
            read_note("read")
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
            ["load_skill", "call_tool"]
        );
        if matches!(req.messages.last(), Some(Message::Tool(result)) if result.call_id == "read".into())
        {
            assert_eq!(
                req.messages,
                &[
                    Message::User(PROMPT.into()),
                    Message::Assistant(load("load")),
                    Message::Tool(wakefold::ToolResult::new("load", definitions())),
                    Message::Summary(SUMMARY.into()),
                    Message::Assistant(read_note("read")),
                    Message::Tool(wakefold::ToolResult::new("read", json!("read"))),
                ]
            );
            assert_eq!(
                req.assistant_context,
                &[Some(json!("ctx")), Some(json!("ctx"))]
            );
            return Ok(read_note("after-restart"));
        }
        Ok(ModelResponse::Final("done".into()))
    }
}

fn run_note(
    path: &Path,
    provider: &dyn Provider,
    budget: usize,
) -> Result<wakefold::RunReport, RunError> {
    let mut store = WorkspaceStore::open(path).unwrap();
    let mut journal = store.open_journal("chat", "notes").unwrap();
    block_on(journal.run(
        PROMPT,
        provider,
        &[Skill::new(
            "notes",
            "Manage notes",
            &[&WriteNote, &ReadNote],
        )],
        RunOptions::new(5).with_context_budget(budget),
    ))
}

#[test]
fn process_restart_restores_loaded_tools_after_compaction() {
    if let Some(path) = std::env::var_os(CHILD_STATE) {
        panic!(
            "expected process exit during next model request, got {:?}",
            run_note(Path::new(&path), &BeforeRestart, 1000)
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
    let report = run_note(dir.path(), &AfterRestart, 2000).unwrap();
    assert_eq!(report.answer, "done");
    assert_eq!(report.usage.reported, TokenUsage::new(8, 4));
    assert_eq!(report.usage.unreported_turns, 2);
    assert_eq!(run_note(dir.path(), &NoCalls, 2000).unwrap(), report);

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
                    ["load_skill", "call_tool"]
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
    let loader = ModelResponse::ToolCall(ToolCall::new(
        "first",
        "skills.load",
        json!({"name": "notes"}),
    ));
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
                    RunEvent::ModelCallCompleted(ModelTurn::new(
                        ModelResponse::ToolCall(ToolCall::new("write", "notes.write", json!({}))),
                        None,
                    )),
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
                ["load_skill", "call_tool"]
            );
            Ok(ModelResponse::Final("saved".into()))
        }
    }
    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let mut journal = store.open_journal("chat", "old").unwrap();
    let skills = [Skill::new("notes", "Manage notes", &[&WriteNote])];
    let result = block_on(journal.run(
        PROMPT,
        &NoCalls,
        &skills,
        RunOptions::new(3).with_context_budget(1),
    ));
    let Err(RunError::ContextBudgetExceeded { required, .. }) = result else {
        panic!("expected context to include restored definitions, got {result:?}");
    };
    let report = block_on(journal.run(
        PROMPT,
        &Finish,
        &skills,
        RunOptions::new(3).with_context_budget(required.try_into().unwrap()),
    ))
    .unwrap();
    assert_eq!(report.answer, "saved");
}

#[test]
fn an_older_checkpoint_restores_definitions_before_dispatching() {
    struct Resume;
    #[async_trait]
    impl Provider for Resume {
        async fn complete(&self, req: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
            assert_eq!(
                req.tools
                    .iter()
                    .map(|spec| spec.name.as_str())
                    .collect::<Vec<_>>(),
                ["load_skill", "call_tool"]
            );
            let Message::Tool(result) = &req.messages[2] else {
                panic!("restored loader definitions should follow the prompt");
            };
            assert_eq!(result.call_id, "wakefold-load-1".into());
            assert_eq!(result.value["tools"][0]["name"], "notes.write");
            assert_eq!(
                result.value["tools"][0]["parameters"],
                WriteNote.spec().parameters
            );
            if matches!(req.messages.last(), Some(Message::Tool(result)) if result.value == "written")
            {
                return Ok(ModelResponse::Final("saved".into()));
            }
            Ok(write_note())
        }
    }

    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let mut journal = store.open_journal("chat", "old-checkpoint").unwrap();
    let checkpoint = serde_json::from_value(json!({
        "state": {
            "messages": [Message::User(PROMPT.into()), Message::Summary("Notes loaded.".into())],
            "assistant_context": [],
            "used_call_ids": ["wakefold-load-0"],
            "model_turns": 1,
            "usage": wakefold::RunUsage::default()
        },
        "loaded_skills": ["notes"]
    }))
    .unwrap();
    journal
        .append(0, &[RunEvent::Compacted(checkpoint)])
        .unwrap();
    let skills = [Skill::new("notes", "Manage notes", &[&WriteNote])];
    let report = block_on(journal.run(PROMPT, &Resume, &skills, RunOptions::new(3))).unwrap();
    assert_eq!(report.answer, "saved");
    assert_eq!(
        block_on(journal.run(PROMPT, &NoCalls, &skills, RunOptions::new(3))).unwrap(),
        report
    );
}

#[test]
fn concurrent_driver_is_excluded_until_the_owner_dies() {
    const CHILD: &str = "WAKEFOLD_RUN_OWNER_TEST";
    struct HoldAtModel<'a>(&'a Path);
    #[async_trait]
    impl Provider for HoldAtModel<'_> {
        async fn complete(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
            fs::write(self.0.join("waiting"), b"").unwrap();
            // Bound the lifetime even if the parent test fails.
            std::thread::sleep(std::time::Duration::from_secs(15));
            panic!("parent did not terminate the test worker")
        }
    }
    struct Answer;
    #[async_trait]
    impl Provider for Answer {
        async fn complete(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
            Ok(ModelResponse::Final("done".into()))
        }
    }

    if let Some(dir) = std::env::var_os(CHILD) {
        let path = Path::new(&dir);
        let mut store = WorkspaceStore::open(path).unwrap();
        let mut journal = store.open_journal("chat", "one").unwrap();
        block_on(journal.run(PROMPT, &HoldAtModel(path), &[], RunOptions::new(1))).unwrap();
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "concurrent_driver_is_excluded_until_the_owner_dies",
        ])
        .env(CHILD, dir.path())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !dir.path().join("waiting").exists() {
        assert!(
            start.elapsed().as_secs() < 10,
            "child did not enter its provider"
        );
        assert!(owner.try_wait().unwrap().is_none(), "child exited early");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let mut competing = store.open_journal("chat", "one").unwrap();
    let result = block_on(competing.run(PROMPT, &NoCalls, &[], RunOptions::new(1)));
    let other = block_on(store.open_journal("chat", "two").unwrap().run(
        PROMPT,
        &Answer,
        &[],
        RunOptions::new(1),
    ));
    owner.kill().unwrap();
    owner.wait().unwrap();
    assert!(
        matches!(
            result,
            Err(RunError::Journal(
                wakefold::JournalError::AlreadyRunning { .. }
            ))
        ),
        "{result:?}"
    );
    assert_eq!(other.unwrap().answer, "done");
    let mut recovered = store.open_journal("chat", "one").unwrap();
    assert_eq!(
        block_on(recovered.run(PROMPT, &Answer, &[], RunOptions::new(1)))
            .unwrap()
            .answer,
        "done"
    );
}
