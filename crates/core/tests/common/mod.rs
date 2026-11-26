//! Script the loader exchange that precedes calls to a skill's tools.

use serde_json::json;
use wakefold::testing::ScriptedTurn;
use wakefold::{JsonValue, Message, ModelResponse, TokenUsage, ToolCall, ToolResult, ToolSpec};

pub fn load_skill(user: &Message, name: &str, tools: &[ToolSpec]) -> (ScriptedTurn, Vec<Message>) {
    let call = ModelResponse::from(ToolCall::new("load", "load_skill", json!({"name": name})));
    let turn =
        ScriptedTurn::new(vec![user.clone()], call.clone()).with_usage(TokenUsage::default());
    let messages = vec![
        user.clone(),
        Message::Assistant(call),
        Message::Tool(ToolResult::new("load", definitions(name, tools))),
    ];
    (turn, messages)
}

pub fn definitions(name: &str, tools: &[ToolSpec]) -> JsonValue {
    let tools = tools
        .iter()
        .cloned()
        .map(|mut spec| {
            spec.name = format!("{name}.{}", spec.name);
            spec
        })
        .collect::<Vec<_>>();
    json!({"loaded": name, "tools": tools})
}

pub fn call_tool(id: &str, tool_name: &str, arguments: JsonValue) -> ToolCall {
    ToolCall::new(
        id,
        "call_tool",
        json!({"tool_name": tool_name, "arguments": arguments}),
    )
}
