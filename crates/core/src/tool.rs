//! Tool registration, execution, and call correlation.

use std::fmt;

use async_trait::async_trait;
use thiserror::Error;

use crate::JsonValue;

/// Identifies a model-requested tool call within a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallId(String);

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

/// A registered tool and the shape of its accepted arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolSpec {
    /// Name used by the provider to request the tool.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema describing the tool's arguments.
    pub parameters: JsonValue,
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
        }
    }
}

/// A request from the model to invoke a named tool.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// A failure to run a tool.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ToolError {
    message: String,
}

impl ToolError {
    /// Create an error with a diagnostic message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
