use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    JsonValue, Message, ModelResponse, RunError, Tool, ToolCall, ToolError, ToolResult, ToolSpec,
    run,
};

#[derive(Default)]
struct Echo {
    calls: AtomicUsize,
}

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("echo", "Return the arguments", json!({"type": "object"}))
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(arguments.clone())
    }
}

#[test]
fn returns_the_tool_result_to_the_provider_with_the_same_call_id() -> Result<(), RunError> {
    let prompt = "Echo the value";
    let user = Message::User(prompt.to_owned());
    let args = json!({"value": 42});
    let call = ModelResponse::ToolCall(ToolCall::new("call-1", "echo", args.clone()));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new(vec![user.clone()], call.clone()),
        ScriptedTurn::new(
            vec![
                user,
                Message::Assistant(call),
                Message::Tool(ToolResult::new("call-1", args)),
            ],
            ModelResponse::Final(String::from("The value is 42.")),
        ),
    ]);
    let tool = Echo::default();

    let answer = block_on(run(prompt, &provider, &[&tool], 2))?;
    assert_eq!(answer, "The value is 42.");
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn stops_before_running_a_tool_when_no_follow_up_turn_remains() {
    let prompt = "Echo the value";
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::ToolCall(ToolCall::new("call-1", "echo", json!({"value": 42}))),
    )]);
    let tool = Echo::default();

    let result = block_on(run(prompt, &provider, &[&tool], 1));

    assert!(
        matches!(result, Err(RunError::TurnLimitReached { limit: 1 })),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn rejects_a_repeated_call_id_without_running_the_tool_again() {
    let prompt = "Echo the value";
    let user = Message::User(prompt.to_owned());
    let args = json!({"value": 42});
    let call = ModelResponse::ToolCall(ToolCall::new("repeated", "echo", args.clone()));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new(vec![user.clone()], call.clone()),
        ScriptedTurn::new(
            vec![
                user,
                Message::Assistant(call.clone()),
                Message::Tool(ToolResult::new("repeated", args)),
            ],
            call,
        ),
    ]);
    let tool = Echo::default();

    let result = block_on(run(prompt, &provider, &[&tool], 3));

    assert!(
        matches!(result, Err(RunError::DuplicateCallId { .. })),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn rejects_an_unknown_tool_without_calling_a_registered_tool() {
    let prompt = "Echo the value";
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::ToolCall(ToolCall::new("call-1", "missing", json!({}))),
    )]);
    let tool = Echo::default();

    let result = block_on(run(prompt, &provider, &[&tool], 2));

    assert!(
        matches!(&result, Err(RunError::UnknownTool { name }) if name == "missing"),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
}
