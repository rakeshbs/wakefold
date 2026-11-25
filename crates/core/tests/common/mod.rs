//! Script the loader exchange that precedes calls to a skill's tools.

use serde_json::json;
use wakefold::testing::ScriptedTurn;
use wakefold::{Message, ModelResponse, TokenUsage, ToolCall, ToolResult};

pub fn load_skill(user: &Message, name: &str) -> (ScriptedTurn, Vec<Message>) {
    let call = ModelResponse::ToolCall(ToolCall::new("load", "skills.load", json!({"name": name})));
    let turn =
        ScriptedTurn::new(vec![user.clone()], call.clone()).with_usage(TokenUsage::default());
    let messages = vec![
        user.clone(),
        Message::Assistant(call),
        Message::Tool(ToolResult::new("load", json!({"loaded": name}))),
    ];
    (turn, messages)
}
