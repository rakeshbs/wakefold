mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    JsonValue, Message, ModelResponse, RunError, RunOptions, Skill, TokenUsage, Tool, ToolCall,
    ToolError, ToolResult, ToolSpec, run, run_with_options,
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
fn returns_the_tool_result_and_accumulates_usage_across_turns() -> Result<(), RunError> {
    let prompt = "Echo the value";
    let user = Message::User(prompt.to_owned());
    let args = json!({"value": 42});
    let call = ModelResponse::from(common::call_tool("call-1", "util.echo", args.clone()));
    let (load, mut messages) = common::load_skill(&user, "util", &[Echo::default().spec()]);
    let call_turn =
        ScriptedTurn::new(messages.clone(), call.clone()).with_usage(TokenUsage::new(20, 5));
    messages.push(Message::Assistant(call.clone()));
    messages.push(Message::Tool(ToolResult::new("call-1", args)));
    let provider = ScriptedProvider::new([
        load,
        call_turn,
        ScriptedTurn::new(
            messages,
            ModelResponse::Final(String::from("The value is 42.")),
        )
        .with_usage(TokenUsage::new(53, 22)),
    ]);
    let tool = Echo::default();

    let options = RunOptions::new(3).with_token_budget(100);
    let report = block_on(run_with_options(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        options,
    ))?;
    assert_eq!(report.answer, "The value is 42.");
    assert_eq!(report.usage.reported_tokens, TokenUsage::new(73, 27));
    assert_eq!(report.usage.reported_tokens.total_tokens(), 100);
    assert_eq!(report.usage.model_calls_without_usage, 0);
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn resumes_a_saved_single_tool_call_from_an_older_journal() -> Result<(), Box<dyn std::error::Error>>
{
    use wakefold::{RunEvent, WorkspaceStore};

    let dir = tempfile::tempdir()?;
    let prompt = "Load the echo skill";
    let user = Message::User(prompt.into());
    let tool = Echo::default();
    let (_, messages) = common::load_skill(&user, "util", &[tool.spec()]);
    {
        let mut store = WorkspaceStore::open(dir.path())?;
        let workspace = store.open_run_workspace("chat", "run")?;
        store.open_journal("chat", "run")?.append(
            0,
            &[
                RunEvent::RunStarted(prompt.into()),
                RunEvent::ModelCallRequested,
            ],
        )?;
        // Write the old event format directly so the current serializer cannot normalize it.
        let event = json!({
            "kind": "model_call_completed",
            "data": {
                "response": {"ToolCall": {"id": "load", "name": "load_skill", "arguments": {"name": "util"}}},
                "usage": null,
                "context": null
            }
        });
        rusqlite::Connection::open(dir.path().join("wakefold.sqlite3"))?.execute(
            "INSERT INTO run_events(agent_id, sequence, event_json) VALUES (?1, 3, ?2)",
            rusqlite::params![workspace.agent_id(), event.to_string()],
        )?;
    }

    let mut store = WorkspaceStore::open(dir.path())?;
    let mut journal = store.open_journal("chat", "run")?;
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        messages,
        ModelResponse::Final("ready".into()),
    )]);
    let report = block_on(journal.run_or_resume(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        RunOptions::new(2),
    ))?;
    assert_eq!(report.answer, "ready");
    let events = journal.load_after(2)?;
    let RunEvent::ModelCallCompleted(turn) = &events[0].event else {
        panic!("expected the saved turn")
    };
    assert_eq!(
        serde_json::to_value(&turn.response)?,
        json!({
            "ToolCalls": {"text": null, "calls": [{"id": "load", "name": "load_skill", "arguments": {"name": "util"}}]}
        })
    );
    Ok(())
}

#[test]
fn rejects_a_tool_turn_with_neither_calls_nor_text() {
    for text in [None, Some(String::new())] {
        let provider = ScriptedProvider::new([ScriptedTurn::new(
            vec![Message::User("prompt".into())],
            ModelResponse::ToolCalls {
                text,
                calls: vec![],
            },
        )]);
        assert!(
            matches!(block_on(run("prompt", &provider, &[], 1)), Err(RunError::Provider(error)) if error.to_string().contains("neither tool calls nor text"))
        );
    }
}

#[test]
fn stops_before_running_a_tool_when_no_follow_up_turn_remains() {
    let prompt = "Echo the value";
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::from(common::call_tool(
            "call-1",
            "util.echo",
            json!({"value": 42}),
        )),
    )]);
    let tool = Echo::default();

    let result = block_on(run(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        1,
    ));

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
    let call = ModelResponse::from(common::call_tool("repeated", "util.echo", args.clone()));
    let (load, mut messages) = common::load_skill(&user, "util", &[Echo::default().spec()]);
    let call_turn = ScriptedTurn::new(messages.clone(), call.clone());
    messages.push(Message::Assistant(call.clone()));
    messages.push(Message::Tool(ToolResult::new("repeated", args)));
    let provider = ScriptedProvider::new([load, call_turn, ScriptedTurn::new(messages, call)]);
    let tool = Echo::default();

    let result = block_on(run(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        4,
    ));

    assert!(
        matches!(result, Err(RunError::DuplicateToolCallId { .. })),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn rejects_a_tool_from_an_unloaded_skill() {
    let prompt = "Echo the value";
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::from(common::call_tool("call-1", "util.echo", json!({}))),
    )]);
    let tool = Echo::default();

    let result = block_on(run(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        2,
    ));

    assert!(
        matches!(&result, Err(RunError::ToolUnavailable { name }) if name == "util.echo"),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn stops_before_a_tool_or_follow_up_when_usage_reaches_the_budget() {
    let prompt = "Echo the value";
    let turn = ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::from(common::call_tool(
            "call-1",
            "util.echo",
            json!({"value": 42}),
        )),
    )
    .with_usage(TokenUsage::new(92, 8));

    for limit in [99, 100] {
        let provider = ScriptedProvider::new([turn.clone()]);
        let tool = Echo::default();
        let options = RunOptions::new(2).with_token_budget(limit);

        let result = block_on(run_with_options(
            prompt,
            &provider,
            &[Skill::new("util", "Utility tools", &[&tool])],
            options,
        ));

        assert!(
            matches!(&result, Err(RunError::TokenBudgetReached { limit: reached, usage })
                if *reached == limit && usage.reported_tokens == TokenUsage::new(92, 8) && usage.model_calls_without_usage == 0),
            "{result:?}"
        );
        assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn zero_budget_stops_before_the_first_model_call() {
    let provider = ScriptedProvider::new([]);
    let options = RunOptions::new(1).with_token_budget(0);

    let result = block_on(run_with_options("Hello", &provider, &[], options));

    assert!(
        matches!(&result, Err(RunError::TokenBudgetReached { limit: 0, usage })
            if usage.reported_tokens.total_tokens() == 0 && usage.model_calls_without_usage == 0),
        "{result:?}"
    );
}

#[test]
fn budgeted_run_stops_when_a_tool_turn_has_no_usage_report() {
    let prompt = "Echo the value";
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::from(common::call_tool(
            "call-1",
            "util.echo",
            json!({"value": 42}),
        )),
    )]);
    let tool = Echo::default();
    let options = RunOptions::new(2).with_token_budget(100);

    let result = block_on(run_with_options(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        options,
    ));

    assert!(
        matches!(&result, Err(RunError::TokenUsageUnavailable { usage })
            if usage.reported_tokens.total_tokens() == 0 && usage.model_calls_without_usage == 1),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn reported_zero_allows_continuation_and_missing_final_usage_is_counted() -> Result<(), RunError> {
    let prompt = "Echo an empty object";
    let user = Message::User(prompt.to_owned());
    let call = ModelResponse::from(common::call_tool("call-1", "util.echo", json!({})));
    let (load, mut messages) = common::load_skill(&user, "util", &[Echo::default().spec()]);
    let call_turn =
        ScriptedTurn::new(messages.clone(), call.clone()).with_usage(TokenUsage::default());
    messages.push(Message::Assistant(call.clone()));
    messages.push(Message::Tool(ToolResult::new("call-1", json!({}))));
    let provider = ScriptedProvider::new([
        load,
        call_turn,
        ScriptedTurn::new(messages, ModelResponse::Final(String::from("Done."))),
    ]);
    let tool = Echo::default();
    let options = RunOptions::new(3).with_token_budget(1);

    let report = block_on(run_with_options(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        options,
    ))?;

    assert_eq!(report.answer, "Done.");
    assert_eq!(report.usage.reported_tokens, TokenUsage::default());
    assert_eq!(report.usage.model_calls_without_usage, 1);
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn rejects_an_oversized_prompt_before_calling_the_provider() {
    let provider = ScriptedProvider::new([]);
    let error = block_on(run_with_options(
        "café",
        &provider,
        &[],
        RunOptions::new(1).with_context_byte_budget(4),
    ))
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "the transcript requires 5 bytes, exceeding the 4-byte budget"
    );
    assert!(matches!(
        error,
        RunError::ContextBudgetExceeded {
            limit_bytes: 4,
            required_bytes: 5,
        }
    ));
}

#[test]
fn compacted_tool_call_ids_still_reject_duplicates() {
    let prompt = "Echo twice.";
    let user = Message::User(String::from(prompt));
    let args = json!({"value": 42});
    let first_call = common::call_tool("first", "util.echo", args.clone());
    let first_turn = ModelResponse::ToolCalls {
        text: Some("a".repeat(30)),
        calls: vec![first_call.clone()],
    };
    let second_turn = ModelResponse::ToolCalls {
        text: Some("b".repeat(600)),
        calls: vec![common::call_tool("second", "util.echo", args.clone())],
    };
    let (load, mut first_exchange) = common::load_skill(&user, "util", &[Echo::default().spec()]);
    let first_request = first_exchange.clone();
    let pinned = first_exchange[1..].to_vec();
    first_exchange.push(Message::Assistant(first_turn.clone()));
    first_exchange.push(Message::Tool(ToolResult::new("first", args.clone())));
    let mut summary_input = first_exchange.clone();
    summary_input.push(Message::User(String::from(concat!(
        "Summarize the earlier work for continuing the original task. ",
        "Preserve relevant tool facts, decisions, and next steps. ",
        "Return only the summary, up to 59 UTF-8 bytes."
    ))));
    let summary = String::from("First echo returned 42.");
    let mut compacted = vec![user];
    compacted.extend(pinned);
    compacted.extend([
        Message::Summary(summary.clone()),
        Message::Assistant(second_turn.clone()),
        Message::Tool(ToolResult::new("second", args.clone())),
    ]);
    let provider = ScriptedProvider::new([
        load,
        ScriptedTurn::new(first_request, first_turn),
        ScriptedTurn::new(first_exchange, second_turn.clone()),
        ScriptedTurn::new(summary_input, ModelResponse::Final(summary.clone())),
        ScriptedTurn::new(compacted, ModelResponse::from(first_call)),
    ]);
    let tool = Echo::default();

    let result = block_on(run_with_options(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        RunOptions::new(5).with_context_byte_budget(902),
    ));

    assert!(
        matches!(&result, Err(RunError::DuplicateToolCallId { id }) if id == &"first".into()),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 2);
}

#[test]
fn loading_twice_returns_the_same_definitions() -> Result<(), RunError> {
    let user = Message::User("Use the echo skill".into());
    let (load, mut messages) = common::load_skill(&user, "first", &[Echo::default().spec()]);
    let loaded_tools = ["load_skill", "call_tool"];
    let again = ModelResponse::from(ToolCall::new(
        "again",
        "load_skill",
        json!({"name": "first"}),
    ));
    let repeat =
        ScriptedTurn::new(messages.clone(), again.clone()).expect_tool_names(&loaded_tools);
    messages.push(Message::Assistant(again));
    messages.push(Message::Tool(ToolResult::new(
        "again",
        common::definitions("first", &[Echo::default().spec()]),
    )));
    let done = ScriptedTurn::new(messages, ModelResponse::Final("done".into()))
        .expect_tool_names(&loaded_tools);
    let provider = ScriptedProvider::new([load.expect_tool_names(&loaded_tools), repeat, done]);
    let first = Echo::default();
    let second = Echo::default();
    let answer = block_on(run(
        "Use the echo skill",
        &provider,
        &[
            Skill::new("first", "First echo", &[&first]),
            Skill::new("second", "Second echo", &[&second]),
        ],
        3,
    ))?;
    assert_eq!(answer, "done");
    Ok(())
}

#[test]
fn a_load_and_premature_tool_call_in_one_turn_are_rejected_before_dispatch() {
    let echo = Echo::default();
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User("test".into())],
        ModelResponse::ToolCalls {
            text: None,
            calls: vec![
                ToolCall::new("load", "load_skill", json!({"name": "util"})),
                common::call_tool("early", "util.echo", json!({})),
            ],
        },
    )]);
    let result = block_on(run(
        "test",
        &provider,
        &[Skill::new("util", "Echo", &[&echo])],
        2,
    ));
    assert!(matches!(result, Err(RunError::ToolUnavailable { name }) if name == "util.echo"));
    assert_eq!(echo.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn loader_rejects_unknown_skills_and_invalid_arguments() {
    for args in [
        json!({"name": "other"}),
        json!({"name": 1}),
        json!(["skills"]),
        json!({}),
        json!({"name": "skills", "extra": 0}),
    ] {
        let provider = ScriptedProvider::new([ScriptedTurn::new(
            vec![Message::User("test".into())],
            ModelResponse::from(ToolCall::new("load", "load_skill", args)),
        )]);
        let result = block_on(run("test", &provider, &[], 2));
        assert!(matches!(result, Err(RunError::Tool { name, .. }) if name == "load_skill"));
    }
}

#[test]
fn rejects_malformed_and_unregistered_tool_calls_before_execution() {
    for (name, arguments) in [
        ("call_tool", json!({})),
        ("call_tool", json!({"tool_name": "util.echo"})),
        ("call_tool", json!({"tool_name": 1, "arguments": {}})),
        (
            "call_tool",
            json!({"tool_name": "util.echo", "arguments": {}, "extra": true}),
        ),
        (
            "call_tool",
            json!({"tool_name": "other.echo", "arguments": {}}),
        ),
        (
            "call_tool",
            json!({"tool_name": "load_skill", "arguments": {"name": "util"}}),
        ),
        (
            "call_tool",
            json!({"tool_name": "call_tool", "arguments": {}}),
        ),
        ("util.echo", json!({})),
        ("skills.load", json!({"name": "util"})),
    ] {
        let user = Message::User("test".into());
        let (load, messages) = common::load_skill(&user, "util", &[Echo::default().spec()]);
        let provider = ScriptedProvider::new([
            load,
            ScriptedTurn::new(
                messages,
                ModelResponse::from(ToolCall::new("bad", name, arguments)),
            ),
        ]);
        let tool = Echo::default();
        let result = block_on(run(
            "test",
            &provider,
            &[Skill::new("util", "Echo", &[&tool])],
            3,
        ));
        assert!(
            matches!(
                result,
                Err(RunError::Tool { .. } | RunError::ToolUnavailable { .. })
            ),
            "{result:?}"
        );
        assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
    }
}

#[cfg(feature = "derive")]
#[test]
fn dispatcher_deserializes_inner_arguments_before_running_a_typed_tool() {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use wakefold::{ToolInput, typed_tool};

    #[derive(Deserialize, JsonSchema, ToolInput)]
    #[serde(deny_unknown_fields)]
    #[tool(name = "echo", description = "Echo a message")]
    struct EchoInput {
        message: String,
    }

    let calls = AtomicUsize::new(0);
    let tool = typed_tool(|input: EchoInput| {
        calls.fetch_add(1, Ordering::Relaxed);
        async move { Ok(json!(input.message)) }
    });
    let user = Message::User("test".into());
    let (load, messages) = common::load_skill(&user, "util", &[tool.spec()]);
    let provider = ScriptedProvider::new([
        load,
        ScriptedTurn::new(
            messages,
            ModelResponse::from(common::call_tool(
                "bad",
                "util.echo",
                json!({"message": 42}),
            )),
        ),
    ]);
    let result = block_on(run(
        "test",
        &provider,
        &[Skill::new("util", "Echo", &[&tool])],
        3,
    ));
    assert!(matches!(result, Err(RunError::Tool { .. })));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn dispatcher_preserves_non_object_tool_arguments() -> Result<(), RunError> {
    use async_trait::async_trait;

    struct TextTool;
    #[async_trait]
    impl Tool for TextTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("text", "Return a string", json!({"type": "string"}))
        }

        async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
            let text = arguments
                .as_str()
                .ok_or_else(|| ToolError::new("expected a string"))?;
            Ok(json!(text))
        }
    }

    let user = Message::User("Echo text".into());
    let (load, mut messages) = common::load_skill(&user, "util", &[TextTool.spec()]);
    let call = ModelResponse::from(common::call_tool("text", "util.text", json!("hello")));
    let turn = ScriptedTurn::new(messages.clone(), call.clone());
    messages.push(Message::Assistant(call));
    messages.push(Message::Tool(ToolResult::new("text", json!("hello"))));
    let provider = ScriptedProvider::new([
        load,
        turn,
        ScriptedTurn::new(messages, ModelResponse::Final("hello".into())),
    ]);
    assert_eq!(
        block_on(run(
            "Echo text",
            &provider,
            &[Skill::new("util", "Text", &[&TextTool])],
            3
        ))?,
        "hello"
    );
    Ok(())
}

#[test]
fn rejects_ambiguous_registrations_before_the_first_turn() {
    let provider = ScriptedProvider::new([]);
    let tool = Echo::default();
    let skill = Skill::new("util", "Echo", &[]);
    assert!(matches!(
        block_on(run("", &provider, &[skill, skill], 1)),
        Err(RunError::DuplicateSkillName { .. })
    ));
    assert!(
        matches!(block_on(run("", &provider, &[Skill::new("util", "Echo", &[&tool, &tool])], 1)),
        Err(RunError::DuplicateToolName { name }) if name == "util.echo")
    );
    assert!(matches!(
        block_on(run(
            "",
            &provider,
            &[Skill::new("a.b", "Ambiguous name", &[])],
            1
        )),
        Err(RunError::InvalidName { .. })
    ));
}
