use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    JsonValue, Message, ModelResponse, RunError, RunOptions, TokenUsage, Tool, ToolCall, ToolError,
    ToolResult, ToolSpec, run, run_with_options,
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
    let call = ModelResponse::ToolCall(ToolCall::new("call-1", "echo", args.clone()));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new(vec![user.clone()], call.clone()).with_usage(TokenUsage::new(20, 5)),
        ScriptedTurn::new(
            vec![
                user,
                Message::Assistant(call),
                Message::Tool(ToolResult::new("call-1", args)),
            ],
            ModelResponse::Final(String::from("The value is 42.")),
        )
        .with_usage(TokenUsage::new(53, 22)),
    ]);
    let tool = Echo::default();

    let options = RunOptions::new(2).with_token_budget(100);
    let report = block_on(run_with_options(prompt, &provider, &[&tool], options))?;
    assert_eq!(report.answer, "The value is 42.");
    assert_eq!(report.usage.reported, TokenUsage::new(73, 27));
    assert_eq!(report.usage.reported.total_tokens(), 100);
    assert_eq!(report.usage.unreported_turns, 0);
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

#[test]
fn stops_before_a_tool_or_follow_up_when_usage_reaches_the_budget() {
    let prompt = "Echo the value";
    let turn = ScriptedTurn::new(
        vec![Message::User(prompt.to_owned())],
        ModelResponse::ToolCall(ToolCall::new("call-1", "echo", json!({"value": 42}))),
    )
    .with_usage(TokenUsage::new(92, 8));

    for limit in [99, 100] {
        let provider = ScriptedProvider::new([turn.clone()]);
        let tool = Echo::default();
        let options = RunOptions::new(2).with_token_budget(limit);

        let result = block_on(run_with_options(prompt, &provider, &[&tool], options));

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
        ModelResponse::ToolCall(ToolCall::new("call-1", "echo", json!({"value": 42}))),
    )]);
    let tool = Echo::default();
    let options = RunOptions::new(2).with_token_budget(100);

    let result = block_on(run_with_options(prompt, &provider, &[&tool], options));

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
    let call = ModelResponse::ToolCall(ToolCall::new("call-1", "echo", json!({})));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new(vec![user.clone()], call.clone()).with_usage(TokenUsage::default()),
        ScriptedTurn::new(
            vec![
                user,
                Message::Assistant(call),
                Message::Tool(ToolResult::new("call-1", json!({}))),
            ],
            ModelResponse::Final(String::from("Done.")),
        ),
    ]);
    let tool = Echo::default();
    let options = RunOptions::new(2).with_token_budget(1);

    let report = block_on(run_with_options(prompt, &provider, &[&tool], options))?;

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
    let first_call = ToolCall::new("first", "echo", args.clone());
    let first_turn = ModelResponse::ToolCalls {
        text: Some("a".repeat(30)),
        calls: vec![first_call.clone()],
    };
    let second_turn = ModelResponse::ToolCalls {
        text: Some("b".repeat(200)),
        calls: vec![ToolCall::new("second", "echo", args.clone())],
    };
    let first_exchange = vec![
        user.clone(),
        Message::Assistant(first_turn.clone()),
        Message::Tool(ToolResult::new("first", args.clone())),
    ];
    let mut summary_input = first_exchange.clone();
    summary_input.push(Message::User(String::from(concat!(
        "Summarize the earlier work for continuing the original task. ",
        "Preserve relevant tool facts, decisions, and next steps. ",
        "Return only the summary, up to 49 UTF-8 bytes."
    ))));
    let summary = String::from("First echo returned 42.");
    let provider = ScriptedProvider::new([
        ScriptedTurn::new(vec![user.clone()], first_turn),
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
        &[&tool],
        RunOptions::new(4).with_context_budget(300),
    ));

    assert!(
        matches!(&result, Err(RunError::DuplicateCallId { id }) if id == &"first".into()),
        "{result:?}"
    );
    assert_eq!(tool.calls.load(Ordering::Relaxed), 2);
}
