//! Core library for the Wakefold agent harness.

mod agent;
mod context;
mod journal;
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
    RunCheckpoint, RunError, RunOptions, RunReport, RunUsage, run, run_streaming,
    run_streaming_with_options, run_with_options,
};
pub use journal::{JournalEntry, JournalError, RunEvent, RunJournal};
pub use provider::{
    Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, TokenUsage,
};
#[cfg(feature = "openai")]
pub use providers::openai::{OpenAiProvider, ReasoningEffort};
pub use retry::ModelRetryPolicy;
pub use serde_json::Value as JsonValue;
pub use skill::Skill;
pub use tokio_util::sync::CancellationToken;
pub use tool::{
    Tool, ToolCall, ToolCallId, ToolContext, ToolError, ToolRecovery, ToolResult, ToolSpec,
};
#[cfg(feature = "derive")]
pub use typed_tool::{ToolInput, typed_tool, typed_tool_with_context};
#[cfg(feature = "derive")]
pub use wakefold_macros::ToolInput;
pub use workspace::{RunWorkspace, WorkspaceError, WorkspaceStore};
