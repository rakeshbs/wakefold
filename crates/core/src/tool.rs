//! Tool registration, execution, and call correlation.

use std::fmt;

use async_trait::async_trait;
use thiserror::Error;

use crate::{CancellationToken, JsonValue, RunWorkspace};

/// Identifies a model-requested tool call within a run.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ToolCallId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ToolCallId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Controls how a journaled run resumes a tool call with no saved result.
///
/// The policy applies whenever the journal recorded the call but has no completed
/// result. This can happen after a process crash, cancellation, or a handler
/// returning [`ToolError`], even if the host resumes in the same process.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolRecovery {
    /// Retry with the original call key when both the saved and current policies allow it.
    ///
    /// Calls that only read data can repeat directly. Calls that change state
    /// must use [`ToolContext::tool_call_key`] to prevent duplicate changes
    /// and recover the original result.
    RetryAfterCrash,
    /// Stop with an unknown outcome if the call's result was never saved.
    #[default]
    StopAfterCrash,
}

impl ToolRecovery {
    fn is_default(&self) -> bool {
        *self == Self::StopAfterCrash
    }
}

/// A registered tool and the shape of its accepted arguments.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolSpec {
    /// Name used by the provider to request the tool.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema describing the tool's arguments.
    #[serde(rename = "parameters")]
    pub argument_schema: JsonValue,
    /// Recovery policy for interrupted calls. The handler must uphold this declaration.
    #[serde(default, skip_serializing_if = "ToolRecovery::is_default")]
    pub recovery: ToolRecovery,
}

impl ToolSpec {
    /// Describe a tool and its expected arguments.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        argument_schema: JsonValue,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            argument_schema,
            recovery: ToolRecovery::StopAfterCrash,
        }
    }

    /// Set the recovery policy for an interrupted call.
    #[must_use]
    pub fn with_recovery(mut self, recovery: ToolRecovery) -> Self {
        self.recovery = recovery;
        self
    }
}

/// A request from the model to invoke a named tool.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolCall {
    /// Identifier assigned by the provider.
    pub id: ToolCallId,
    /// Name of the registered tool to invoke.
    pub name: String,
    /// Arguments supplied by the model.
    pub arguments: JsonValue,
}

impl ToolCall {
    /// Create a call with the identifier and arguments supplied by a provider.
    pub fn new(id: impl Into<ToolCallId>, name: impl Into<String>, arguments: JsonValue) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

/// The output of a tool call and the ID it answers.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolResult {
    /// ID copied from the original call.
    pub call_id: ToolCallId,
    /// Output returned by the tool.
    pub value: JsonValue,
}

impl ToolResult {
    /// Associate a tool output with the call that produced it.
    pub fn new(call_id: impl Into<ToolCallId>, value: JsonValue) -> Self {
        Self {
            call_id: call_id.into(),
            value,
        }
    }
}

/// Run information available to a tool handler.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    cancellation: Option<CancellationToken>,
    workspace: Option<RunWorkspace>,
    tool_call_key: Option<String>,
}

impl ToolContext {
    pub(crate) fn new(
        cancellation: Option<CancellationToken>,
        workspace: Option<RunWorkspace>,
        tool_call_key: Option<String>,
    ) -> Self {
        Self {
            cancellation,
            workspace,
            tool_call_key,
        }
    }

    /// The workspace supplied for this run, when configured.
    pub fn workspace(&self) -> Option<&RunWorkspace> {
        self.workspace.as_ref()
    }

    /// The caller's cancellation signal, when supplied.
    pub fn cancellation_token(&self) -> Option<&CancellationToken> {
        self.cancellation.as_ref()
    }

    /// The stable key assigned to this call before the agent executes its handler.
    ///
    /// Journaled runs persist this key and reuse it after restart. A handler
    /// that changes state and opts into retries must pass it as an idempotency
    /// key to its backing service or record it with the change in one transaction.
    /// The default context has no assigned key.
    pub fn tool_call_key(&self) -> Option<&str> {
        self.tool_call_key.as_deref()
    }
}

/// A tool that can be registered with the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Describe the tool to the model.
    fn spec(&self) -> ToolSpec;

    /// Run the tool with arguments supplied by the model.
    ///
    /// Implementations should validate the arguments before using them.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] if the arguments are invalid or the tool fails.
    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError>;

    /// Run the tool with access to the run context.
    ///
    /// Tools that coordinate cancellation can observe the signal in `context`.
    /// Use drop guards for cleanup that must run when the agent drops the call.
    ///
    /// # Errors
    ///
    /// Returns a [`ToolError`] if the arguments are invalid or the tool fails.
    async fn call_with_context(
        &self,
        _context: ToolContext,
        arguments: &JsonValue,
    ) -> Result<JsonValue, ToolError> {
        self.call(arguments).await
    }
}

/// A failure to run a tool.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ToolError {
    message: String,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ToolError {
    /// Create an error with a diagnostic message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// Create an error that preserves the underlying tool failure.
    pub fn with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}
