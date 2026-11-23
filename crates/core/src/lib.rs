//! Core library for the Wakefold agent harness.

mod agent;
mod provider;
mod tool;

#[cfg(feature = "openai")]
mod openai;

#[cfg(feature = "test-util")]
pub mod testing;

pub use agent::{RunError, run, run_streaming};
#[cfg(feature = "openai")]
pub use openai::OpenAiProvider;
pub use provider::{Message, ModelRequest, ModelResponse, Provider, ProviderError};
pub use serde_json::Value as JsonValue;
pub use tool::{CallId, Tool, ToolCall, ToolError, ToolResult, ToolSpec};
