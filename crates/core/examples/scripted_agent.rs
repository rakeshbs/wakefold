use std::num::NonZeroU64;

use futures_executor::block_on;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    JsonValue, Message, ModelResponse, RunError, ToolCall, ToolError, ToolResult, WakefoldTool,
    run, typed_tool,
};

#[derive(Deserialize, JsonSchema, WakefoldTool)]
#[serde(deny_unknown_fields)]
#[tool(
    name = "lookup_price",
    description = "Look up the price of a catalog item in cents"
)]
struct LookupPrice {
    item: String,
}

async fn lookup_price(input: LookupPrice) -> Result<JsonValue, ToolError> {
    if input.item != "notebook" {
        return Err(ToolError::new("expected the notebook catalog item"));
    }

    Ok(json!({"price_cents": 350}))
}

#[derive(Deserialize, JsonSchema, WakefoldTool)]
#[serde(deny_unknown_fields)]
#[tool(
    name = "calculate_total",
    description = "Multiply an item price in cents by a quantity"
)]
struct CalculateTotal {
    unit_price_cents: u64,
    quantity: NonZeroU64,
}

async fn calculate_total(input: CalculateTotal) -> Result<JsonValue, ToolError> {
    let total = input
        .unit_price_cents
        .checked_mul(input.quantity.get())
        .ok_or_else(|| ToolError::new("total in cents exceeds the supported range"))?;

    Ok(json!({"total_cents": total}))
}

fn notebook_total() -> Result<String, RunError> {
    let prompt = "What do three notebooks cost?";
    let lookup = ModelResponse::ToolCall(ToolCall::new(
        "price-1",
        "lookup_price",
        json!({"item": "notebook"}),
    ));
    let calculate = ModelResponse::ToolCall(ToolCall::new(
        "total-1",
        "calculate_total",
        json!({"unit_price_cents": 350, "quantity": 3}),
    ));

    let mut transcript = vec![Message::User(prompt.to_owned())];
    let lookup_turn = ScriptedTurn::new(transcript.clone(), lookup.clone());
    transcript.extend([
        Message::Assistant(lookup),
        Message::Tool(ToolResult::new("price-1", json!({"price_cents": 350}))),
    ]);
    let calculate_turn = ScriptedTurn::new(transcript.clone(), calculate.clone());
    transcript.extend([
        Message::Assistant(calculate),
        Message::Tool(ToolResult::new("total-1", json!({"total_cents": 1050}))),
    ]);

    let provider = ScriptedProvider::new([
        lookup_turn,
        calculate_turn,
        ScriptedTurn::new(
            transcript,
            ModelResponse::Final(String::from("Three notebooks cost $10.50.")),
        ),
    ]);

    let lookup_tool = typed_tool(lookup_price);
    let total_tool = typed_tool(calculate_total);
    block_on(run(prompt, &provider, &[&lookup_tool, &total_tool], 3))
}

fn main() -> Result<(), RunError> {
    let answer = notebook_total()?;
    println!("{answer}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::notebook_total;
    use wakefold::RunError;

    #[test]
    fn looks_up_the_unit_price_before_calculating_the_total() -> Result<(), RunError> {
        assert_eq!(notebook_total()?, "Three notebooks cost $10.50.");
        Ok(())
    }
}
