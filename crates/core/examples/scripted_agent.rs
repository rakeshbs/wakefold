use std::num::NonZeroU64;

use futures_executor::block_on;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    JsonValue, Message, ModelResponse, RunError, RunOptions, RunReport, TokenUsage, ToolCall,
    ToolError, ToolResult, WakefoldTool, run_with_options, typed_tool,
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

fn notebook_total() -> Result<RunReport, RunError> {
    let prompt = "What do three notebooks cost?";
    let user = Message::User(prompt.to_owned());
    let lookup = ModelResponse::ToolCalls {
        text: Some(String::from(
            "I’ll start by checking the catalog for the price of one notebook.",
        )),
        calls: vec![ToolCall::new(
            "price-1",
            "lookup_price",
            json!({"item": "notebook"}),
        )],
    };
    let calculate = ModelResponse::ToolCalls {
        text: Some(String::from(concat!(
            "I’ll use the catalog’s unit price of 350 cents and the requested quantity of three. ",
            "I’ll ask the calculator to multiply those values, then use the returned total in cents ",
            "to format the answer in dollars and cents."
        ))),
        calls: vec![ToolCall::new(
            "total-1",
            "calculate_total",
            json!({"unit_price_cents": 350, "quantity": 3}),
        )],
    };

    let lookup_turn = ScriptedTurn::new(vec![user.clone()], lookup.clone());
    let earlier = vec![
        user.clone(),
        Message::Assistant(lookup),
        Message::Tool(ToolResult::new("price-1", json!({"price_cents": 350}))),
    ];
    let calculate_turn = ScriptedTurn::new(earlier.clone(), calculate.clone());

    // The calculator exchange pushes the next request over budget. The model
    // summarizes the earlier lookup and keeps the calculator exchange intact.
    let mut summary_request = earlier;
    summary_request.push(Message::User(String::from(concat!(
        "Summarize the earlier work for continuing the original task. ",
        "Preserve relevant tool facts, decisions, and next steps. ",
        "Return only the summary, up to 66 UTF-8 bytes."
    ))));
    let summary = String::from("Catalog lookup confirmed 350 cents per notebook.");
    let summary_turn = ScriptedTurn::new(summary_request, ModelResponse::Final(summary.clone()))
        .with_usage(TokenUsage::new(80, 12));
    let final_turn = ScriptedTurn::new(
        vec![
            user,
            Message::Summary(summary),
            Message::Assistant(calculate),
            Message::Tool(ToolResult::new("total-1", json!({"total_cents": 1050}))),
        ],
        ModelResponse::Final(String::from("Three notebooks cost $10.50.")),
    );

    let provider = ScriptedProvider::new([lookup_turn, calculate_turn, summary_turn, final_turn]);
    let lookup_tool = typed_tool(lookup_price);
    let total_tool = typed_tool(calculate_total);
    let options = RunOptions::new(3).with_context_budget(400);
    block_on(run_with_options(
        prompt,
        &provider,
        &[&lookup_tool, &total_tool],
        options,
    ))
}

fn main() -> Result<(), RunError> {
    let report = notebook_total()?;
    println!("{}", report.answer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::notebook_total;
    use wakefold::{RunError, TokenUsage};

    #[test]
    fn calculates_the_total_after_compacting_context() -> Result<(), RunError> {
        let report = notebook_total()?;
        assert_eq!(report.answer, "Three notebooks cost $10.50.");
        assert_eq!(report.usage.reported, TokenUsage::new(80, 12));
        Ok(())
    }
}
