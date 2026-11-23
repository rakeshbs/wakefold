//! Chat Completions provider for OpenAI and compatible servers.

use std::collections::BTreeMap;
use std::fmt;

use async_openai::Client;
use async_openai::config::{OPENAI_API_BASE, OpenAIConfig};
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCallChunk,
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestMessage,
    ChatCompletionRequestToolMessage, ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
    ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequest, FinishReason, FunctionCall, FunctionObject,
};
use async_trait::async_trait;
use futures_util::StreamExt;

use crate::{
    Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, TokenUsage, ToolCall,
};

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
    client: Client<OpenAIConfig>,
    include_usage: bool,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("model", &self.model)
            .field("include_usage", &self.include_usage)
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
            include_usage: false,
        })
    }

    /// Request token usage from a compatible endpoint that supports it.
    pub fn with_usage_reporting(mut self) -> Self {
        self.include_usage = true;
        self
    }

    fn chat_request(&self, request: ModelRequest<'_>) -> CreateChatCompletionRequest {
        let tools = request
            .tools
            .iter()
            .map(|spec| {
                ChatCompletionTools::Function(ChatCompletionTool {
                    function: FunctionObject {
                        name: spec.name.clone(),
                        description: Some(spec.description.clone()),
                        parameters: Some(spec.parameters.clone()),
                        strict: None,
                    },
                })
            })
            .collect::<Vec<_>>();

        CreateChatCompletionRequest {
            model: self.model.clone(),
            messages: request.messages.iter().map(chat_message).collect(),
            tools: (!tools.is_empty()).then_some(tools),
            stream: Some(true),
            stream_options: self.include_usage.then_some(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            }),
            ..Default::default()
        }
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
        let mut stream = self
            .client
            .chat()
            .create_stream(self.chat_request(request))
            .await
            .map_err(|source| ProviderError::with_source("could not open chat stream", source))?;
        let mut text = String::new();
        let mut calls = BTreeMap::<u32, ToolCallParts>::new();
        let mut response = None;
        let mut usage = None;

        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|source| ProviderError::with_source("chat stream failed", source))?;
            if let Some(reported) = chunk.usage {
                usage = Some(TokenUsage::new(
                    u64::from(reported.prompt_tokens),
                    u64::from(reported.completion_tokens),
                ));
            }
            if response.is_none() {
                for choice in chunk.choices.into_iter().filter(|choice| choice.index == 0) {
                    if let Some(delta) = choice.delta.content.or(choice.delta.refusal) {
                        text.push_str(&delta);
                        if !delta.is_empty() {
                            on_text(&delta);
                        }
                    }
                    for delta in choice.delta.tool_calls.into_iter().flatten() {
                        calls.entry(delta.index).or_default().append(delta)?;
                    }
                    if let Some(reason) = choice.finish_reason {
                        response = Some(finish_turn(
                            reason,
                            std::mem::take(&mut text),
                            std::mem::take(&mut calls),
                        )?);
                        break;
                    }
                }
            }
            if response.is_some() && (!self.include_usage || usage.is_some()) {
                break;
            }
        }

        let response = response
            .ok_or_else(|| ProviderError::new("chat stream ended before a finish reason"))?;
        Ok(ModelTurn::new(response, usage))
    }
}

fn chat_message(message: &Message) -> ChatCompletionRequestMessage {
    match message {
        Message::User(prompt) => {
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(prompt.clone()),
                name: None,
            })
        }
        Message::Assistant(response) => {
            let calls = response
                .tool_calls()
                .iter()
                .map(|call| {
                    ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                        id: call.id.to_string(),
                        function: FunctionCall {
                            name: call.name.clone(),
                            arguments: call.arguments.to_string(),
                        },
                    })
                })
                .collect::<Vec<_>>();
            ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                content: response.text().map(|text| {
                    ChatCompletionRequestAssistantMessageContent::Text(text.to_owned())
                }),
                tool_calls: (!calls.is_empty()).then_some(calls),
                ..Default::default()
            })
        }
        Message::Tool(result) => {
            ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
                tool_call_id: result.call_id.to_string(),
                content: ChatCompletionRequestToolMessageContent::Text(result.value.to_string()),
            })
        }
    }
}

#[derive(Default)]
struct ToolCallParts {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallParts {
    fn append(&mut self, delta: ChatCompletionMessageToolCallChunk) -> Result<(), ProviderError> {
        merge_tool_field(&mut self.id, delta.id, "ID")?;
        if let Some(function) = delta.function {
            merge_tool_field(&mut self.name, function.name, "name")?;
            if let Some(arguments) = function.arguments {
                self.arguments.push_str(&arguments);
            }
        }
        Ok(())
    }

    fn into_call(self) -> Result<ToolCall, ProviderError> {
        let id = self
            .id
            .ok_or_else(|| ProviderError::new("tool call is missing its ID"))?;
        let name = self
            .name
            .ok_or_else(|| ProviderError::new("tool call is missing its name"))?;
        let arguments = serde_json::from_str(&self.arguments).map_err(|source| {
            ProviderError::with_source(format!("tool call {id} contains invalid JSON"), source)
        })?;
        Ok(ToolCall::new(id, name, arguments))
    }
}

fn merge_tool_field(
    current: &mut Option<String>,
    update: Option<String>,
    field: &str,
) -> Result<(), ProviderError> {
    if let Some(update) = update.filter(|value| !value.is_empty()) {
        if current.as_ref().is_some_and(|value| value != &update) {
            return Err(ProviderError::new(format!("tool call changed its {field}")));
        }
        *current = Some(update);
    }
    Ok(())
}

fn finish_turn(
    reason: FinishReason,
    text: String,
    calls: BTreeMap<u32, ToolCallParts>,
) -> Result<ModelResponse, ProviderError> {
    match reason {
        FinishReason::Stop if calls.is_empty() => Ok(ModelResponse::Final(text)),
        FinishReason::ToolCalls if !calls.is_empty() => {
            let calls = calls
                .into_values()
                .map(ToolCallParts::into_call)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ModelResponse::ToolCalls {
                text: (!text.is_empty()).then_some(text),
                calls,
            })
        }
        FinishReason::Length => Err(ProviderError::new(
            "chat completion reached its token limit",
        )),
        FinishReason::ContentFilter => Err(ProviderError::new("chat completion was filtered")),
        FinishReason::FunctionCall => {
            Err(ProviderError::new("legacy function calls are unsupported"))
        }
        FinishReason::Stop | FinishReason::ToolCalls => Err(ProviderError::new(
            "chat finish reason did not match the tool calls received",
        )),
    }
}

#[cfg(test)]
mod tests;
