//! Core library for the Wakefold agent harness.

mod agent;
mod context;
mod provider;
mod providers;
mod retry;
mod skill;
mod tool;
mod workspace;

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
pub use providers::openai::{OpenAiProvider, ReasoningEffort};
pub use retry::RetryPolicy;
pub use serde_json::Value as JsonValue;
pub use skill::Skill;
pub use tokio_util::sync::CancellationToken;
pub use tool::{CallId, Tool, ToolCall, ToolContext, ToolError, ToolResult, ToolSpec};
#[cfg(feature = "derive")]
pub use typed_tool::{ToolInput, typed_tool, typed_tool_with_context};
#[cfg(feature = "derive")]
pub use wakefold_macros::WakefoldTool;
pub use workspace::{RunWorkspace, WorkspaceError, WorkspaceStore};
