//! Core library for the Wakefold agent harness.

mod agent;
mod provider;
mod providers;
mod tool;

#[cfg(feature = "derive")]
mod typed_tool;

#[cfg(feature = "test-util")]
pub mod testing;

#[cfg(feature = "derive")]
extern crate self as wakefold;

pub use agent::{
    RunError, RunOptions, RunReport, RunUsage, run, run_streaming, run_streaming_with_options,
    run_with_options,
};
pub use provider::{
    Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, TokenUsage,
};
#[cfg(feature = "openai")]
pub use providers::openai::OpenAiProvider;
pub use serde_json::Value as JsonValue;
pub use tool::{CallId, Tool, ToolCall, ToolError, ToolResult, ToolSpec};
#[cfg(feature = "derive")]
pub use typed_tool::{ToolInput, typed_tool};
#[cfg(feature = "derive")]
pub use wakefold_macros::WakefoldTool;
