//! Registration of asynchronous tool handlers with typed inputs.

use std::future::Future;
use std::marker::PhantomData;

use async_trait::async_trait;
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;

use crate::{JsonValue, Tool, ToolContext, ToolError, ToolSpec};

/// Model-facing metadata for the argument type of a tool.
pub trait ToolInput {
    /// Name the model uses to select the tool.
    const NAME: &'static str;

    /// Description of what the tool does.
    const DESCRIPTION: &'static str;
}

/// Register an asynchronous handler using its input type as the tool schema.
///
/// Derive [`crate::WakefoldTool`], Serde's `Deserialize`, and Schemars's
/// `JsonSchema` on a named-field struct. Arguments are deserialized before
/// the handler runs.
///
/// ```
/// use schemars::JsonSchema;
/// use serde::Deserialize;
/// use wakefold::{JsonValue, Tool, WakefoldTool, typed_tool};
///
/// #[derive(Deserialize, JsonSchema, WakefoldTool)]
/// #[tool(name = "echo", description = "Echo a message")]
/// struct Echo {
///     message: String,
/// }
///
/// let echo = typed_tool(|input: Echo| async move {
///     Ok(JsonValue::String(input.message))
/// });
/// assert_eq!(echo.spec().name, "echo");
/// ```
pub fn typed_tool<Input, Handler, Output>(handler: Handler) -> impl Tool
where
    Input: ToolInput + DeserializeOwned + JsonSchema + Send + 'static,
    Handler: Fn(Input) -> Output + Send + Sync,
    Output: Future<Output = Result<JsonValue, ToolError>> + Send,
{
    typed_tool_with_context(move |input, _context| handler(input))
}

/// Register a typed asynchronous handler that also receives the run context.
///
/// Use [`ToolContext::cancellation_token`] to observe the caller's signal.
pub fn typed_tool_with_context<Input, Handler, Output>(handler: Handler) -> impl Tool
where
    Input: ToolInput + DeserializeOwned + JsonSchema + Send + 'static,
    Handler: Fn(Input, ToolContext) -> Output + Send + Sync,
    Output: Future<Output = Result<JsonValue, ToolError>> + Send,
{
    TypedTool {
        handler,
        input: PhantomData::<fn() -> Input>,
    }
}

struct TypedTool<Input, Handler> {
    handler: Handler,
    input: PhantomData<fn() -> Input>,
}

#[async_trait]
impl<Input, Handler, Output> Tool for TypedTool<Input, Handler>
where
    Input: ToolInput + DeserializeOwned + JsonSchema + Send + 'static,
    Handler: Fn(Input, ToolContext) -> Output + Send + Sync,
    Output: Future<Output = Result<JsonValue, ToolError>> + Send,
{
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            Input::NAME,
            Input::DESCRIPTION,
            schema_for!(Input).to_value(),
        )
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        self.call_with_context(ToolContext::default(), arguments)
            .await
    }

    async fn call_with_context(
        &self,
        context: ToolContext,
        arguments: &JsonValue,
    ) -> Result<JsonValue, ToolError> {
        let input = Input::deserialize(arguments)
            .map_err(|error| ToolError::new(format!("invalid tool arguments: {error}")))?;
        (self.handler)(input, context).await
    }
}
