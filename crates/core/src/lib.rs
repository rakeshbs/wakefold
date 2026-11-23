//! Core library for the Wakefold agent harness.

mod agent;
mod provider;
mod tool;

#[cfg(feature = "derive")]
mod typed_tool;

#[cfg(feature = "openai")]
mod openai;

#[cfg(feature = "test-util")]
pub mod testing;

#[cfg(feature = "derive")]
extern crate self as wakefold;

pub use agent::{RunError, run, run_streaming};
#[cfg(feature = "openai")]
pub use openai::OpenAiProvider;
pub use provider::{Message, ModelRequest, ModelResponse, Provider, ProviderError};
pub use serde_json::Value as JsonValue;
pub use tool::{CallId, Tool, ToolCall, ToolError, ToolResult, ToolSpec};
#[cfg(feature = "derive")]
pub use typed_tool::{ToolInput, typed_tool};
#[cfg(feature = "derive")]
pub use wakefold_macros::WakefoldTool;
