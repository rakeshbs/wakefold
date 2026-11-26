use std::{fs, path::Path, process::Command};

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    JsonValue, Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, RunError,
    RunEvent, RunOptions, Skill, TokenUsage, Tool, ToolCall, ToolContext, ToolError, ToolRecovery,
    ToolSpec, WorkspaceStore,
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
            .chat_root()
            .join(workspace.agent_relative_directory())
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
            .chat_root()
            .join(workspace.agent_relative_directory())
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
    ModelResponse::from(ToolCall::new(id, "load_skill", json!({"name": "notes"})))
}

fn write_note() -> ModelResponse {
    ModelResponse::ToolCalls {
        text: Some("x".repeat(200)),
        calls: vec![ToolCall::new(
            "write",
            "call_tool",
            json!({"tool_name": "notes.write", "arguments": {}}),
        )],
    }
}

fn read_note(id: &str) -> ModelResponse {
    ModelResponse::ToolCalls {
        text: Some("y".repeat(500)),
        calls: vec![ToolCall::new(
            id,
            "call_tool",
            json!({"tool_name": "notes.read", "arguments": {}}),
        )],
    }
}

struct BeforeRestart;

#[async_trait]
impl Provider for BeforeRestart {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver requests full model turns")
    }

    async fn stream_turn(
        &self,
        req: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let response = if req.tools.is_empty() {
            ModelResponse::ToolCalls {
                text: Some(SUMMARY.into()),
                calls: vec![],
            }
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
        turn.replay_data = Some(json!("ctx"));
        Ok(turn)
    }
}

struct AfterRestart;

#[async_trait]
impl Provider for AfterRestart {
    async fn complete_response(
        &self,
        req: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
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
                req.assistant_replay_data,
                &[Some(json!("ctx")), Some(json!("ctx"))]
            );
            return Ok(read_note("after-restart"));
        }
        Ok(ModelResponse::ToolCalls {
            text: Some("done".into()),
            calls: vec![],
        })
    }
}

fn run_note(
    path: &Path,
    provider: &dyn Provider,
    budget: usize,
) -> Result<wakefold::RunReport, RunError> {
    let mut store = WorkspaceStore::open(path).unwrap();
    let mut journal = store.open_journal("chat", "notes").unwrap();
    block_on(journal.run_or_resume(
        PROMPT,
        provider,
        &[Skill::new(
            "notes",
            "Manage notes",
            &[&WriteNote, &ReadNote],
        )],
        RunOptions::new(5).with_context_byte_budget(budget),
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
    assert_eq!(report.usage.reported_tokens, TokenUsage::new(8, 4));
    assert_eq!(report.usage.model_calls_without_usage, 2);
    assert_eq!(run_note(dir.path(), &NoCalls, 2000).unwrap(), report);

    // Check the bytes written by the run before compatibility decoding can change their shape.
    let db = rusqlite::Connection::open(dir.path().join("wakefold.sqlite3")).unwrap();
    let saved_response: String = db
        .query_row(
            "SELECT json_extract(event_json, '$.data.response') FROM run_events
         WHERE json_extract(event_json, '$.kind') = 'model_call_completed'
         ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<JsonValue>(&saved_response).unwrap(),
        json!({"Final": "done"})
    );

    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let mut journal = store.open_journal("chat", "notes").unwrap();
    assert!(matches!(
        block_on(journal.run_or_resume("different", &NoCalls, &[], RunOptions::new(5))),
        Err(RunError::ResumePromptMismatch)
    ));
    let entries = journal.load_after(0).unwrap();
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
            .filter(|e| matches!(e.event, RunEvent::TranscriptCompacted(_)))
            .count(),
        1
    );
}

struct NoCalls;

#[async_trait]
impl Provider for NoCalls {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        panic!("unexpected provider call during recovery")
    }
}

#[test]
fn resume_distinguishes_a_wrong_workspace_from_invalid_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let other = store.open_run_workspace("chat", "other").unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    assert!(matches!(
        block_on(journal.run_or_resume(
            PROMPT,
            &NoCalls,
            &[],
            RunOptions::new(1).with_workspace(other)
        )),
        Err(RunError::RunWorkspaceMismatch)
    ));
    journal.append(0, &[RunEvent::ModelCallRequested]).unwrap();
    assert!(matches!(
        block_on(journal.run_or_resume(PROMPT, &NoCalls, &[], RunOptions::new(1))),
        Err(RunError::InvalidJournal(_))
    ));
}

struct LoadTwice;

#[async_trait]
impl Provider for LoadTwice {
    async fn complete_response(
        &self,
        req: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
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
    let result = block_on(journal.run_or_resume(PROMPT, &LoadTwice, &skills, RunOptions::new(3)));
    assert!(matches!(result, Err(RunError::Journal(_))));
    let entries = journal.load_after(0).unwrap();
    assert!(matches!(
        entries.last().unwrap().event,
        RunEvent::ToolCallPrepared { .. }
    ));
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry.event, RunEvent::SkillLoaded(_)))
    );
    db.execute_batch("DROP TRIGGER interrupt_loader").unwrap();
    let report =
        block_on(journal.run_or_resume(PROMPT, &LoadTwice, &skills, RunOptions::new(3))).unwrap();
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
        block_on(journal.run_or_resume("different", &NoCalls, &skills, RunOptions::new(3))),
        Err(RunError::ResumePromptMismatch)
    ));
    assert!(matches!(
        block_on(journal.run_or_resume(PROMPT, &NoCalls, &[], RunOptions::new(3))),
        Err(RunError::Tool { .. })
    ));
}

#[test]
fn stop_after_crash_returns_unknown_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(dir.path()).unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    let loader = load("first");
    // Older journals recorded the dispatcher target under `name`.
    let call = ModelResponse::from(ToolCall::new(
        "write",
        "call_tool",
        json!({"name": "notes.write", "arguments": {}}),
    ));
    journal
        .append(
            0,
            &[
                RunEvent::RunStarted(PROMPT.into()),
                RunEvent::ModelCallRequested,
                RunEvent::ModelCallCompleted(ModelTurn::new(loader.clone(), None)),
                RunEvent::ToolCallPrepared {
                    call: loader.tool_calls()[0].clone(),
                    recovery: ToolRecovery::RetryAfterCrash,
                    tool_call_key: "load-key".into(),
                },
                RunEvent::SkillLoaded("notes".into()),
                RunEvent::ToolCallCompleted(wakefold::ToolResult::new(
                    "first",
                    json!({"loaded": "notes", "tools": [{
                        "name": "notes.write", "description": "Write the note",
                        "parameters": {"type": "object"}
                    }]}),
                )),
                RunEvent::ModelCallRequested,
                RunEvent::ModelCallCompleted(ModelTurn::new(call.clone(), None)),
                RunEvent::ToolCallPrepared {
                    call: call.tool_calls()[0].clone(),
                    recovery: ToolRecovery::StopAfterCrash,
                    tool_call_key: "write-key".into(),
                },
            ],
        )
        .unwrap();
    let result = block_on(journal.run_or_resume(
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
fn concurrent_driver_is_excluded_until_the_owner_dies() {
    const CHILD: &str = "WAKEFOLD_RUN_OWNER_TEST";
    struct HoldAtModel<'a>(&'a Path);
    #[async_trait]
    impl Provider for HoldAtModel<'_> {
        async fn complete_response(
            &self,
            _: ModelRequest<'_>,
        ) -> Result<ModelResponse, ProviderError> {
            fs::write(self.0.join("waiting"), b"").unwrap();
            // Bound the lifetime even if the parent test fails.
            std::thread::sleep(std::time::Duration::from_secs(15));
            panic!("parent did not terminate the test worker")
        }
    }
    struct Answer;
    #[async_trait]
    impl Provider for Answer {
        async fn complete_response(
            &self,
            _: ModelRequest<'_>,
        ) -> Result<ModelResponse, ProviderError> {
            Ok(ModelResponse::Final("done".into()))
        }
    }

    if let Some(dir) = std::env::var_os(CHILD) {
        let path = Path::new(&dir);
        let mut store = WorkspaceStore::open(path).unwrap();
        let mut journal = store.open_journal("chat", "one").unwrap();
        block_on(journal.run_or_resume(PROMPT, &HoldAtModel(path), &[], RunOptions::new(1)))
            .unwrap();
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
    let result = block_on(competing.run_or_resume(PROMPT, &NoCalls, &[], RunOptions::new(1)));
    let other = block_on(store.open_journal("chat", "two").unwrap().run_or_resume(
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
        block_on(recovered.run_or_resume(PROMPT, &Answer, &[], RunOptions::new(1)))
            .unwrap()
            .answer,
        "done"
    );
}
