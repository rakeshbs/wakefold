mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use futures_executor::block_on;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    Message, ModelResponse, RunOptions, RunWorkspace, Skill, Tool, ToolContext, ToolError,
    ToolResult, WakefoldTool, WorkspaceStore, run_with_options, typed_tool_with_context,
};

const CONTENT: &str = "notes from the first agent";
const CHAT: &str = "../chat/one";

#[derive(Deserialize, JsonSchema, WakefoldTool)]
#[tool(name = "note", description = "Access the chat note")]
struct Note {}

fn run_note_tool(workspace: RunWorkspace, read_from: Option<&Path>) {
    let tool = typed_tool_with_context(|_: Note, ctx: ToolContext| async move {
        let workspace = ctx.workspace().expect("agent should receive its workspace");
        let content = match read_from {
            Some(artifact) => fs::read_to_string(workspace.root().join(artifact)),
            None => {
                let artifact = workspace.agent_directory().join("note.txt");
                fs::write(workspace.root().join(artifact), CONTENT).map(|()| CONTENT.to_owned())
            }
        }
        .map_err(|e| ToolError::new(e.to_string()))?;
        Ok(json!(content))
    });
    let user = Message::User("use the note".into());
    let response = ModelResponse::ToolCalls {
        text: None,
        calls: vec![common::call_tool("1", "notes.note", json!({}))],
    };
    let (load, mut messages) = common::load_skill(&user, "notes", &[tool.spec()]);
    let note = ScriptedTurn::new(messages.clone(), response.clone());
    messages.push(Message::Assistant(response));
    messages.push(Message::Tool(ToolResult::new("1", json!(CONTENT))));
    let provider = ScriptedProvider::new([
        load,
        note,
        ScriptedTurn::new(messages, ModelResponse::Final("done".into())),
    ]);
    block_on(run_with_options(
        "use the note",
        &provider,
        &[Skill::new("notes", "Access chat notes", &[&tool])],
        RunOptions::new(3).with_workspace(workspace),
    ))
    .unwrap();
}

#[test]
fn shared_workspace_survives_restart() {
    const CHILD_STATE: &str = "WAKEFOLD_TEST_CHILD_STATE";
    if let Some(state) = std::env::var_os(CHILD_STATE) {
        let mut store = WorkspaceStore::open(state).unwrap();
        let writer = store.open_run(CHAT, "writer").unwrap();
        let reader = store.open_run(CHAT, "reader").unwrap();
        assert_eq!(writer.root(), reader.root());
        assert_ne!(writer.agent_id(), reader.agent_id());
        let artifact = writer.agent_directory().join("note.txt");
        run_note_tool(writer, None);
        run_note_tool(reader, Some(&artifact));
        return;
    }

    let state = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "shared_workspace_survives_restart",
            "--nocapture",
        ])
        .env(CHILD_STATE, state.path())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "writer and reader process failed: {status}"
    );

    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let writer = store.open_run(CHAT, "writer").unwrap();
    let artifact = writer.agent_directory().join("note.txt");
    let reader = store.open_run(CHAT, "reader").unwrap();
    run_note_tool(reader, Some(&artifact));

    let other = store.open_run("chat-two", "writer").unwrap();
    assert_ne!(other.root(), writer.root());
    assert_eq!(
        fs::read_to_string(other.root().join(&artifact))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    fs::create_dir_all(other.root().join(&artifact).parent().unwrap()).unwrap();
    fs::write(other.root().join(&artifact), "second chat").unwrap();
    assert_eq!(
        fs::read_to_string(writer.root().join(&artifact)).unwrap(),
        CONTENT
    );
}

#[test]
fn concurrent_first_runs_share_one_workspace() {
    let state = tempfile::tempdir().unwrap();
    let start = std::sync::Barrier::new(2);
    let (left, right) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            start.wait();
            let mut first = WorkspaceStore::open(state.path()).unwrap();
            first.open_run("new-chat", "first").unwrap()
        });
        start.wait();
        let mut second = WorkspaceStore::open(state.path()).unwrap();
        let right = second.open_run("new-chat", "second").unwrap();
        let left = worker.join().unwrap();
        assert_eq!(second.open_run("new-chat", "first").unwrap(), left);
        (left, right)
    });
    assert_eq!(left.root(), right.root());
    assert_ne!(left.agent_id(), right.agent_id());
}
