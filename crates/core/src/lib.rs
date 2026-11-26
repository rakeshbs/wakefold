//! Core library for the Wakefold agent harness.

mod agent;
mod artifact;
mod context;
mod delegation;
mod journal;
mod mailbox;
mod provider;
mod providers;
mod queued_run;
mod retry;
mod scheduler;
mod skill;
mod tool;
mod wait;
mod workspace;

#[cfg(feature = "derive")]
mod typed_tool;

#[cfg(feature = "test-util")]
pub mod testing;

#[cfg(feature = "derive")]
extern crate self as wakefold;

pub use agent::{
    RunCheckpoint, RunError, RunOptions, RunOutcome, RunReport, RunUsage, run, run_streaming,
    run_streaming_with_options, run_with_options,
};
pub use artifact::{ArtifactError, ArtifactPublication, ArtifactRevision, PublishArtifact};
pub use delegation::{DelegateTool, DelegatedRun};
pub use journal::{JournalEntry, JournalError, JournalReplay, RunEvent, RunEventPage, RunJournal};
pub use mailbox::{ClientMessageError, SendMessage, WaitForMessage};
pub use provider::{
    Message, ModelRequest, ModelResponse, ModelToolSpec, ModelTurn, Provider, ProviderError,
    TokenUsage,
};
#[cfg(feature = "openai")]
pub use providers::openai::{OpenAiProvider, ReasoningEffort};
pub use queued_run::QueueRunError;
pub use retry::ModelRetryPolicy;
pub use scheduler::{ScheduledRun, ScheduledRunConfig, SchedulerError};
pub use serde_json::Value as JsonValue;
pub use skill::Skill;
pub use tokio_util::sync::CancellationToken;
pub use tool::{
    Tool, ToolCall, ToolCallId, ToolContext, ToolError, ToolGrant, ToolOutput, ToolRecovery,
    ToolResult, ToolSpec,
};
#[cfg(feature = "derive")]
pub use typed_tool::{ToolInput, typed_tool, typed_tool_with_context};
pub use wait::{PendingWait, TimerWait, WaitOn, WakeEvent};
#[cfg(feature = "derive")]
pub use wakefold_macros::ToolInput;
pub use workspace::{RunWorkspace, WorkspaceError, WorkspaceStore};
