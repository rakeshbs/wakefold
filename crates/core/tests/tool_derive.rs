use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_executor::block_on;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use wakefold::{Effect, Tool, ToolError, WakefoldTool, typed_tool};

#[derive(Deserialize, JsonSchema, WakefoldTool)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[tool(
    name = "calculate",
    description = "Calculate an order total",
    effect = "read_only"
)]
struct Order<Input> {
    unit_price: Input,
    quantity: NonZeroU64,
    note: Option<String>,
}

#[test]
fn derives_metadata_and_schema_from_the_argument_type() {
    let tool = typed_tool(|_input: Order<u64>| async { Ok(json!(null)) });

    let spec = tool.spec();
    assert_eq!(spec.name, "calculate");
    assert_eq!(spec.effect, Effect::ReadOnly);
    assert_eq!(spec.description, "Calculate an order total");
    assert_eq!(spec.parameters["type"], "object");
    assert_eq!(spec.parameters["additionalProperties"], false);
    assert_eq!(
        spec.parameters["properties"]["unitPrice"]["type"],
        "integer"
    );
    assert_eq!(spec.parameters["properties"]["quantity"]["minimum"], 1);
    let required = spec.parameters["required"]
        .as_array()
        .expect("the schema contains required arguments");
    assert_eq!(required.len(), 2);
    assert!(required.contains(&json!("unitPrice")));
    assert!(required.contains(&json!("quantity")));
}

#[test]
fn deserializes_before_invoking_the_typed_handler() -> Result<(), ToolError> {
    let calls = &AtomicUsize::new(0);
    let tool = typed_tool(move |input: Order<u64>| async move {
        calls.fetch_add(1, Ordering::Relaxed);
        Ok(json!({
            "unit_price": input.unit_price,
            "quantity": input.quantity,
            "note": input.note,
        }))
    });

    for invalid in [
        json!({"unitPrice": "350", "quantity": 3}),
        json!({"unitPrice": 350, "quantity": 0}),
        json!({"unitPrice": 350}),
        json!({"unitPrice": 350, "quantity": 3, "extra": true}),
    ] {
        let error = block_on(tool.call(&invalid)).expect_err("invalid arguments must fail");
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .is::<serde_json::Error>()
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    let result = block_on(tool.call(&json!({"unitPrice": 350, "quantity": 3})))?;
    assert_eq!(
        result,
        json!({"unit_price": 350, "quantity": 3, "note": null})
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    Ok(())
}
