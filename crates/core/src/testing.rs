//! Deterministic providers for offline tests and examples.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, TokenUsage};

/// A model turn with the transcript expected before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedTurn {
    messages: Vec<Message>,
    turn: ModelTurn,
}

impl ScriptedTurn {
    /// Respond with `response` only when the provider sees these messages.
    pub fn new(messages: Vec<Message>, response: ModelResponse) -> Self {
        Self {
            messages,
            turn: ModelTurn::new(response, None),
        }
    }

    /// Report these token counts when returning this turn.
    pub fn with_usage(mut self, usage: TokenUsage) -> Self {
        self.turn.usage = Some(usage);
        self
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
        Ok(self.complete_turn(request, &mut |_| {}).await?.response)
    }

    async fn complete_turn(
        &self,
        request: ModelRequest<'_>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let scripted = self
            .turns
            .lock()
            .expect("scripted provider holds the lock only while taking a turn")
            .pop_front()
            .ok_or_else(|| ProviderError::new("scripted provider ran out of turns"))?;

        if scripted.messages != request.messages {
            return Err(ProviderError::new(
                "scripted provider received unexpected messages",
            ));
        }

        scripted.turn.response.emit_text(on_text);
        Ok(scripted.turn)
    }
}
