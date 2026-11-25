use serde_json::json;

use super::*;
use crate::{ToolCall, ToolResult};

fn tool_turn(id: &str, commentary: String) -> [Message; 2] {
    [
        Message::Assistant(ModelResponse::ToolCalls {
            text: Some(commentary),
            calls: vec![ToolCall::new(id, "lookup", json!({"item": "notebook"}))],
        }),
        Message::Tool(ToolResult::new(id, json!({"price": 350}))),
    ]
}

#[test]
fn prepares_a_bounded_summary_request_and_preserves_the_recent_exchange() -> Result<(), RunError> {
    let prompt = Message::User(String::from("Compare the notebook prices."));
    let prior_summary = Message::Summary(String::from("Compare the listed prices in cents."));
    let earlier = tool_turn("earlier", "Checking the first price. ".repeat(3));
    let latest = tool_turn("latest", "Checking another price. ".repeat(9));
    let mut messages = vec![prompt.clone(), prior_summary.clone()];
    messages.extend(earlier.clone());
    messages.extend(latest.clone());
    let current_reasoning = Some(json!({"reasoning": "latest"}));
    let mut context = vec![
        Some(json!({"reasoning": "previous".repeat(20)})),
        current_reasoning.clone(),
    ];

    let mut sizes = ContextSizes::default();
    let compaction = sizes
        .prepare(&messages, &context, 400)?
        .expect("the history exceeds the budget");
    let request = compaction.request();
    assert_eq!(
        request.messages[..4],
        [
            prompt.clone(),
            prior_summary,
            earlier[0].clone(),
            earlier[1].clone()
        ]
    );
    assert!(
        matches!(request.messages.last(), Some(Message::User(instruction)) if instruction.contains("Preserve relevant tool facts"))
    );
    assert!(request.tools.is_empty());
    assert!(request.assistant_context.is_empty());
    assert!(request.messages.iter().map(message_bytes).sum::<u128>() <= 400);

    let summary = String::from("The first notebook costs 350 cents.");
    compaction.apply(
        ModelResponse::Final(summary.clone()),
        &mut messages,
        &mut context,
    )?;

    let mut expected = vec![prompt, Message::Summary(summary)];
    expected.extend(latest);
    assert_eq!(messages, expected);
    assert_eq!(context, [current_reasoning]);
    let measured_bytes = |messages: &[Message], context: &[Option<JsonValue>]| {
        messages.iter().map(message_bytes).sum::<u128>()
            + context.iter().flatten().map(json_bytes).sum::<u128>()
    };
    assert_eq!(sizes.total, measured_bytes(&messages, &context));
    assert!(sizes.total <= 400);

    messages.extend(tool_turn("next", "é".repeat(300)));
    context.push(Some(json!({"reasoning": "line one\nline two"})));
    let exact = usize::try_from(measured_bytes(&messages, &context)).unwrap();
    assert!(sizes.prepare(&messages, &context, exact)?.is_none());
    assert!(sizes.prepare(&messages, &context, exact - 1)?.is_some());
    Ok(())
}

#[test]
fn rejects_an_oversized_summary_before_replacing_history() -> Result<(), RunError> {
    let mut messages = vec![Message::User(String::from("Find the notebook."))];
    messages.extend(tool_turn("old", "Checking the price. ".repeat(4)));
    messages.extend(tool_turn(
        "latest",
        "Confirming the catalog entry. ".repeat(8),
    ));
    let mut context = vec![None, None];
    let original = messages.clone();
    let mut sizes = ContextSizes::default();
    let compaction = sizes
        .prepare(&messages, &context, 400)?
        .expect("the history exceeds the budget");

    let result = compaction.apply(
        ModelResponse::Final("too long ".repeat(100)),
        &mut messages,
        &mut context,
    );
    assert!(matches!(
        result,
        Err(RunError::ContextBudgetExceeded { limit: 400, .. })
    ));
    assert_eq!(messages, original);
    assert_eq!(context, [None, None]);
    Ok(())
}

#[test]
fn an_oversized_current_exchange_cannot_be_compacted() {
    let mut messages = vec![Message::User(String::from("Find the notebook."))];
    messages.extend(tool_turn("latest", "Looking up the price.".into()));
    messages[2] = Message::Tool(ToolResult::new("latest", json!({"data": "a".repeat(500)})));
    let context = [Some(json!({"reasoning": "current"}))];

    assert!(matches!(
        ContextSizes::default().prepare(&messages, &context, 100),
        Err(RunError::ContextBudgetExceeded { limit: 100, required }) if required > 500
    ));
}
