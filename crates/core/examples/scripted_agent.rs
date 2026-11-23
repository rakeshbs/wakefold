use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    JsonValue, Message, ModelResponse, RunError, Tool, ToolCall, ToolError, ToolResult, ToolSpec,
    run,
};

struct LookupPrice;

#[async_trait]
impl Tool for LookupPrice {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "lookup_price",
            "Look up the price of a catalog item in cents",
            json!({
                "type": "object",
                "properties": {"item": {"type": "string"}},
                "required": ["item"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        if arguments != &json!({"item": "notebook"}) {
            return Err(ToolError::new("expected the notebook catalog item"));
        }

        Ok(json!({"price_cents": 350}))
    }
}

fn main() -> Result<(), RunError> {
    let prompt = "What does a notebook cost?";
    let user = Message::User(prompt.to_owned());
    let call = ModelResponse::ToolCall(ToolCall::new(
        "price-1",
        "lookup_price",
        json!({"item": "notebook"}),
    ));
    let provider = ScriptedProvider::new([
        ScriptedTurn::new(vec![user.clone()], call.clone()),
        ScriptedTurn::new(
            vec![
                user,
                Message::Assistant(call),
                Message::Tool(ToolResult::new("price-1", json!({"price_cents": 350}))),
            ],
            ModelResponse::Final(String::from("A notebook costs $3.50.")),
        ),
    ]);

    let answer = block_on(run(prompt, &provider, &[&LookupPrice], 2))?;
    println!("{answer}");
    Ok(())
}
