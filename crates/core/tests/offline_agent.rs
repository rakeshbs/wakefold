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
    let call = ModelResponse::ToolCall(ToolCall::new("call-1", "util.echo", args.clone()));
    let (load, mut messages) = common::load_skill(&user, "util");
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
    assert_eq!(report.usage.reported, TokenUsage::new(73, 27));
    assert_eq!(report.usage.reported.total_tokens(), 100);
    assert_eq!(report.usage.unreported_turns, 0);
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
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
        ModelResponse::ToolCall(ToolCall::new("call-1", "util.echo", json!({"value": 42}))),
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
    let call = ModelResponse::ToolCall(ToolCall::new("repeated", "util.echo", args.clone()));
    let (load, mut messages) = common::load_skill(&user, "util");
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
        matches!(result, Err(RunError::DuplicateCallId { .. })),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn rejects_a_tool_from_an_unloaded_skill() {
    let prompt = "Echo the value";
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::ToolCall(ToolCall::new("call-1", "util.echo", json!({}))),
    )]);
    let tool = Echo::default();

    let result = block_on(run(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        2,
    ));

    assert!(
        matches!(&result, Err(RunError::UnknownTool { name }) if name == "util.echo"),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn stops_before_a_tool_or_follow_up_when_usage_reaches_the_budget() {
    let prompt = "Echo the value";
    let turn = ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::ToolCall(ToolCall::new("call-1", "util.echo", json!({"value": 42}))),
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
                if *reached == limit && usage.reported == TokenUsage::new(92, 8) && usage.unreported_turns == 0),
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
            if usage.reported.total_tokens() == 0 && usage.unreported_turns == 0),
        "{result:?}"
    );
}

#[test]
fn budgeted_run_stops_when_a_tool_turn_has_no_usage_report() {
    let prompt = "Echo the value";
    let provider = ScriptedProvider::new([ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::ToolCall(ToolCall::new("call-1", "util.echo", json!({"value": 42}))),
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
            if usage.reported.total_tokens() == 0 && usage.unreported_turns == 1),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn reported_zero_allows_continuation_and_missing_final_usage_is_counted() -> Result<(), RunError> {
    let prompt = "Echo an empty object";
    let user = Message::User(prompt.to_owned());
    let call = ModelResponse::ToolCall(ToolCall::new("call-1", "util.echo", json!({})));
    let (load, mut messages) = common::load_skill(&user, "util");
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
    assert_eq!(report.usage.reported, TokenUsage::default());
    assert_eq!(report.usage.unreported_turns, 1);
    assert_eq!(tool.calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn rejects_an_oversized_prompt_before_calling_the_provider() {
    let provider = ScriptedProvider::new([]);
    let result = block_on(run_with_options(
        "café",
        &provider,
        &[],
        RunOptions::new(1).with_context_budget(4),
    ));

    assert!(matches!(
        result,
        Err(RunError::ContextBudgetExceeded {
            limit: 4,
            required: 5,
        })
    ));
}

#[test]
fn compacted_tool_call_ids_still_reject_duplicates() {
    let prompt = "Echo twice.";
    let user = Message::User(String::from(prompt));
    let args = json!({"value": 42});
    let first_call = ToolCall::new("first", "util.echo", args.clone());
    let first_turn = ModelResponse::ToolCalls {
        text: Some("a".repeat(30)),
        calls: vec![first_call.clone()],
    };
    let second_turn = ModelResponse::ToolCalls {
        text: Some("b".repeat(200)),
        calls: vec![ToolCall::new("second", "util.echo", args.clone())],
    };
    let (load, mut first_exchange) = common::load_skill(&user, "util");
    let first_request = first_exchange.clone();
    first_exchange.push(Message::Assistant(first_turn.clone()));
    first_exchange.push(Message::Tool(ToolResult::new("first", args.clone())));
    let mut summary_input = first_exchange.clone();
    summary_input.push(Message::User(String::from(concat!(
        "Summarize the earlier work for continuing the original task. ",
        "Preserve relevant tool facts, decisions, and next steps. ",
        "Return only the summary, up to 64 UTF-8 bytes."
    ))));
    let summary = String::from("First echo returned 42.");
    let provider = ScriptedProvider::new([
        load,
        ScriptedTurn::new(first_request, first_turn),
        ScriptedTurn::new(first_exchange, second_turn.clone()),
        ScriptedTurn::new(summary_input, ModelResponse::Final(summary.clone())),
        ScriptedTurn::new(
            vec![
                user,
                Message::Summary(summary),
                Message::Assistant(second_turn),
                Message::Tool(ToolResult::new("second", args)),
            ],
            ModelResponse::ToolCall(first_call),
        ),
    ]);
    let tool = Echo::default();

    let result = block_on(run_with_options(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        RunOptions::new(5).with_context_budget(320),
    ));

    assert!(
        matches!(&result, Err(RunError::DuplicateCallId { id }) if id == &"first".into()),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 2);
}

#[test]
fn loading_twice_exposes_only_one_copy_and_leaves_other_skills_unloaded() -> Result<(), RunError> {
    let user = Message::User("Use the echo skill".into());
    let (load, mut messages) = common::load_skill(&user, "first");
    let loaded_tools = ["skills.load", "first.echo"];
    let again = ModelResponse::ToolCall(ToolCall::new(
        "again",
        "skills.load",
        json!({"name": "first"}),
    ));
    let repeat = ScriptedTurn::new(messages.clone(), again.clone()).with_tools(&loaded_tools);
    messages.push(Message::Assistant(again));
    messages.push(Message::Tool(ToolResult::new(
        "again",
        json!({"loaded": "first"}),
    )));
    let done =
        ScriptedTurn::new(messages, ModelResponse::Final("done".into())).with_tools(&loaded_tools);
    let provider = ScriptedProvider::new([load.with_tools(&["skills.load"]), repeat, done]);
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
                ToolCall::new("load", "skills.load", json!({"name": "util"})),
                ToolCall::new("early", "util.echo", json!({})),
            ],
        },
    )]);
    let result = block_on(run(
        "test",
        &provider,
        &[Skill::new("util", "Echo", &[&echo])],
        2,
    ));
    assert!(matches!(result, Err(RunError::UnknownTool { name }) if name == "util.echo"));
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
            ModelResponse::ToolCall(ToolCall::new("load", "skills.load", args)),
        )]);
        let result = block_on(run("test", &provider, &[], 2));
        assert!(matches!(result, Err(RunError::Tool { name, .. }) if name == "skills.load"));
    }
}

#[test]
fn rejects_ambiguous_registrations_before_the_first_turn() {
    let provider = ScriptedProvider::new([]);
    let tool = Echo::default();
    let skill = Skill::new("util", "Echo", &[]);
    for skills in [
        vec![skill, skill],
        vec![Skill::new("skills", "Reserved", &[])],
    ] {
        assert!(matches!(
            block_on(run("", &provider, &skills, 1)),
            Err(RunError::DuplicateSkillName { .. })
        ));
    }
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
