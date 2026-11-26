//! Tool registration, execution, and call correlation.

use std::fmt;

use async_trait::async_trait;
use thiserror::Error;

use crate::{CancellationToken, JsonValue, RunWorkspace};

/// Identifies a model-requested tool call within a run.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallId(String);

impl CallId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for CallId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for CallId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Declares whether an interrupted tool call can safely execute again during recovery.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Effect {
    /// Reads external state and can safely repeat after an interrupted call.
    ReadOnly,
    /// Uses the context's idempotency key to deduplicate effects and return the original result.
    Idempotent,
    /// An interrupted call requires reconciliation before another attempt.
    #[default]
    NonIdempotent,
}

impl Effect {
    fn is_non_idempotent(&self) -> bool {
        *self == Self::NonIdempotent
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
    pub parameters: JsonValue,
    /// Recovery policy for interrupted calls. The handler must uphold this declaration.
    #[serde(default, skip_serializing_if = "Effect::is_non_idempotent")]
    pub effect: Effect,
}

impl ToolSpec {
    /// Describe a tool and its expected arguments.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: JsonValue,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            effect: Effect::NonIdempotent,
        }
    }

    /// Declare how this tool can be retried after a crash.
    ///
    /// An idempotent handler must use [`ToolContext::idempotency_key`] to deduplicate
    /// external effects and recover the original result when called again.
    #[must_use]
    pub fn with_effect(mut self, effect: Effect) -> Self {
        self.effect = effect;
        self
    }
}

/// A request from the model to invoke a named tool.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolCall {
    /// Identifier assigned by the provider.
    pub id: CallId,
    /// Name of the registered tool to invoke.
    pub name: String,
    /// Arguments supplied by the model.
    pub arguments: JsonValue,
}

impl ToolCall {
    /// Create a call with the identifier and arguments supplied by a provider.
    pub fn new(id: impl Into<CallId>, name: impl Into<String>, arguments: JsonValue) -> Self {
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
    pub call_id: CallId,
    /// Output returned by the tool.
    pub value: JsonValue,
}

impl ToolResult {
    /// Associate a tool output with the call that produced it.
    pub fn new(call_id: impl Into<CallId>, value: JsonValue) -> Self {
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
    idempotency_key: Option<String>,
}

impl ToolContext {
    pub(crate) fn new(
        cancellation: Option<CancellationToken>,
        workspace: Option<RunWorkspace>,
        idempotency_key: Option<String>,
    ) -> Self {
        Self {
            cancellation,
            workspace,
            idempotency_key,
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
    /// Journaled runs persist this key and reuse it after restart. Idempotent
    /// handlers must pass it to their backing service or record it with their
    /// effects in one transaction. Direct tool calls using the default context
    /// have no assigned key.
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
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
