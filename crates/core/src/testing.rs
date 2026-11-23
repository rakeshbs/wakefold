//! Deterministic providers for offline tests and examples.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Message, ModelRequest, ModelResponse, Provider, ProviderError};

/// A model turn with the transcript expected before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedTurn {
    messages: Vec<Message>,
    response: ModelResponse,
}

impl ScriptedTurn {
    /// Respond with `response` only when the provider sees these messages.
    pub fn new(messages: Vec<Message>, response: ModelResponse) -> Self {
        Self { messages, response }
    }
}

/// A one-run provider that checks the transcript at every turn.
#[derive(Debug)]
pub struct ScriptedProvider {
    turns: Mutex<VecDeque<ScriptedTurn>>,
}

impl ScriptedProvider {
    /// Schedule model turns in the order they should be requested.
    pub fn new(turns: impl IntoIterator<Item = ScriptedTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, request: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        let turn = self
            .turns
            .lock()
            .expect("scripted provider holds the lock only while taking a turn")
            .pop_front()
            .ok_or_else(|| ProviderError::new("scripted provider ran out of turns"))?;

        if turn.messages != request.messages {
            return Err(ProviderError::new(
                "scripted provider received unexpected messages",
            ));
        }

        Ok(turn.response)
    }
}
