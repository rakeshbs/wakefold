//! Client configuration and provider implementation.

use std::fmt;

use async_openai::Client;
use async_openai::config::{OPENAI_API_BASE, OpenAIConfig};
use async_trait::async_trait;

use super::{request, stream};
use crate::{ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UsageReporting {
    Disabled,
    Enabled,
}

/// A streaming provider using the OpenAI-compatible Chat Completions API.
///
/// Calls must run on a Tokio runtime with I/O enabled. Enable the `openai`
/// Cargo feature to use this adapter.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use wakefold::{OpenAiProvider, RunOptions, run_streaming_with_options};
///
/// let provider = OpenAiProvider::new(
///     std::env::var("OPENAI_MODEL")?,
///     std::env::var("OPENAI_API_KEY")?,
/// )?;
/// let options = RunOptions::new(1).with_token_budget(1_000);
/// let report = run_streaming_with_options("Hello", &provider, &[], options, |text| {
///     print!("{text}");
/// })
/// .await?;
/// println!("Reported tokens: {}", report.usage.reported.total_tokens());
/// println!("Turns without usage: {}", report.usage.unreported_turns);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct OpenAiProvider {
    model: String,
    pub(super) client: Client<OpenAIConfig>,
    usage_reporting: UsageReporting,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("model", &self.model)
            .field(
                "include_usage",
                &(self.usage_reporting == UsageReporting::Enabled),
            )
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Create a provider for OpenAI with streamed usage reporting enabled.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if the key cannot be used as a bearer token.
    pub fn new(
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Ok(Self::with_base_url(model, OPENAI_API_BASE, api_key)?.with_usage_reporting())
    }

    /// Create a provider for a base URL such as `http://localhost:8000/v1`.
    ///
    /// Include the server's API prefix in `base_url`. The client adds
    /// `/chat/completions` to it. Use [`Self::with_usage_reporting`] when
    /// the endpoint supports returning token counts in a streaming request.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if the key cannot be used as a bearer token.
    pub fn with_base_url(
        model: impl Into<String>,
        base_url: &str,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.into();
        if !api_key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ProviderError::new("API key must be a single bearer token"));
        }

        let config = OpenAIConfig::new()
            .with_api_base(base_url.trim_end_matches('/'))
            .with_api_key(api_key)
            .with_org_id("")
            .with_project_id("");
        Ok(Self {
            model: model.into(),
            client: Client::with_config(config),
            usage_reporting: UsageReporting::Disabled,
        })
    }

    /// Request token usage from a compatible endpoint that supports it.
    pub fn with_usage_reporting(mut self) -> Self {
        self.usage_reporting = UsageReporting::Enabled;
        self
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn complete(&self, request: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        self.complete_streaming(request, &mut |_| {}).await
    }

    async fn complete_streaming(
        &self,
        request: ModelRequest<'_>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        Ok(self.complete_turn(request, on_text).await?.response)
    }

    async fn complete_turn(
        &self,
        request: ModelRequest<'_>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let stream = self
            .client
            .chat()
            .create_stream(request::chat_request(
                &self.model,
                request,
                self.usage_reporting,
            ))
            .await
            .map_err(|source| ProviderError::with_source("could not open chat stream", source))?;
        stream::collect_turn(stream, self.usage_reporting, on_text).await
    }
}
