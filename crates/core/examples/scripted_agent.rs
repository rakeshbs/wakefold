use std::num::NonZeroU64;

use futures_executor::block_on;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    JsonValue, Message, ModelResponse, RunError, RunOptions, RunReport, Skill, TokenUsage, Tool,
    ToolCall, ToolError, ToolInput, ToolResult, run_with_options, typed_tool,
};

#[derive(Deserialize, JsonSchema, ToolInput)]
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

#[derive(Deserialize, JsonSchema, ToolInput)]
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
    let lookup_tool = typed_tool(lookup_price);
    let total_tool = typed_tool(calculate_total);
    let mut definitions = vec![lookup_tool.spec(), total_tool.spec()];
    for spec in &mut definitions {
        spec.name = format!("pricing.{}", spec.name);
    }
    let loaded = json!({"loaded": "pricing", "tools": definitions});
    let prompt = "What do three notebooks cost?";
    let user = Message::User(prompt.to_owned());
    let lookup = ModelResponse::ToolCalls {
        text: Some(String::from(
            "I’ll start by checking the catalog for the price of one notebook.",
        )),
        calls: vec![ToolCall::new(
            "price-1",
            "call_tool",
            json!({"tool_name": "pricing.lookup_price", "arguments": {"item": "notebook"}}),
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
            "call_tool",
            json!({"tool_name": "pricing.calculate_total", "arguments": {"unit_price_cents": 350, "quantity": 3}}),
        )],
    };

    let load = ModelResponse::ToolCall(ToolCall::new(
        "load-1",
        "load_skill",
        json!({"name": "pricing"}),
    ));
    let load_turn = ScriptedTurn::new(vec![user.clone()], load.clone())
        .expect_tool_names(&["load_skill", "call_tool"]);
    let loaded_tools = ["load_skill", "call_tool"];
    let mut earlier = vec![
        user.clone(),
        Message::Assistant(load),
        Message::Tool(ToolResult::new("load-1", loaded)),
    ];
    let pinned = earlier[1..].to_vec();
    let lookup_turn =
        ScriptedTurn::new(earlier.clone(), lookup.clone()).expect_tool_names(&loaded_tools);
    earlier.push(Message::Assistant(lookup));
    earlier.push(Message::Tool(ToolResult::new(
        "price-1",
        json!({"price_cents": 350}),
    )));
    let calculate_turn =
        ScriptedTurn::new(earlier.clone(), calculate.clone()).expect_tool_names(&loaded_tools);

    // The calculator exchange pushes the next request over budget. The model
    // summarizes the earlier lookup and keeps the calculator exchange intact.
    let mut summary_request = earlier;
    summary_request.push(Message::User(String::from(concat!(
        "Summarize the earlier work for continuing the original task. ",
        "Preserve relevant tool facts, decisions, and next steps. ",
        "Return only the summary, up to 93 UTF-8 bytes."
    ))));
    let summary = String::from("Catalog lookup confirmed 350 cents per notebook.");
    let summary_turn = ScriptedTurn::new(summary_request, ModelResponse::Final(summary.clone()))
        .with_usage(TokenUsage::new(80, 12));
    let mut final_messages = vec![user];
    final_messages.extend(pinned);
    final_messages.extend([
        Message::Summary(summary),
        Message::Assistant(calculate),
        Message::Tool(ToolResult::new("total-1", json!({"total_cents": 1050}))),
    ]);
    let final_turn = ScriptedTurn::new(
        final_messages,
        ModelResponse::Final(String::from("Three notebooks cost $10.50.")),
    )
    .expect_tool_names(&loaded_tools);

    let provider = ScriptedProvider::new([
        load_turn,
        lookup_turn,
        calculate_turn,
        summary_turn,
        final_turn,
    ]);
    let options = RunOptions::new(4).with_context_byte_budget(1260);
    block_on(run_with_options(
        prompt,
        &provider,
        &[Skill::new(
            "pricing",
            "Look up prices and calculate totals",
            &[&lookup_tool, &total_tool],
        )],
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
        assert_eq!(report.usage.reported_tokens, TokenUsage::new(80, 12));
        Ok(())
    }
}
