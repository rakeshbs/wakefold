//! Provider requests and the messages that make up a run.

use std::error::Error;

use async_trait::async_trait;
use thiserror::Error;

use crate::{ToolCall, ToolResult, ToolSpec};

/// A completed message in the agent's transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// The prompt that starts the run.
    User(String),
    /// A completed model turn.
    Assistant(ModelResponse),
    /// The result of a tool call.
    Tool(ToolResult),
}

/// A completed model turn.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelResponse {
    /// The final answer to the user.
    Final(String),
    /// A request to run a tool before asking the model again.
    ToolCall(ToolCall),
    /// An assistant message that requests several tool calls or includes text.
    ToolCalls {
        /// Text emitted before or alongside the calls.
        text: Option<String>,
        /// Calls in the order the agent should execute them.
        calls: Vec<ToolCall>,
    },
}

impl ModelResponse {
    /// Text emitted by the model during this turn, if any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Final(answer) => Some(answer),
            Self::ToolCall(_) => None,
            Self::ToolCalls { text, .. } => text.as_deref(),
        }
    }

    /// Tool calls requested by the model during this turn.
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self {
            Self::Final(_) => &[],
            Self::ToolCall(call) => std::slice::from_ref(call),
            Self::ToolCalls { calls, .. } => calls,
        }
    }
}

/// The transcript and available tools for one model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelRequest<'a> {
    /// Messages in chronological order, including any tool results.
    pub messages: &'a [Message],
    /// Tools the model can request during this run.
    pub tools: &'a [ToolSpec],
}

/// Supplies model turns to the agent loop.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Produce the next model turn for this transcript.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if the provider cannot produce a turn.
    async fn complete(&self, request: ModelRequest<'_>) -> Result<ModelResponse, ProviderError>;

    /// Produce a model turn and report text as it arrives.
    ///
    /// The default implementation emits the text in one chunk after the model
    /// finishes. A streaming provider can emit multiple chunks before returning
    /// the completed turn.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if the provider cannot complete the turn.
    async fn complete_streaming(
        &self,
        request: ModelRequest<'_>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.complete(request).await?;
        if let Some(text) = response.text().filter(|text| !text.is_empty()) {
            on_text(text);
        }
        Ok(response)
    }
}

/// A failure to obtain a model turn.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ProviderError {
    message: String,
    #[source]
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProviderError {
    /// Create an error with a diagnostic message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// Create an error that preserves the failure reported by the provider.
    pub fn with_source(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}
