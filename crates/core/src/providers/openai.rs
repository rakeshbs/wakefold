//! Streaming Responses API adapter with stateless reasoning replay.

use std::fmt;

use async_openai::Client;
use async_openai::config::{OPENAI_API_BASE, OpenAIConfig};
use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, IncludeEnum, InputItem,
    InputParam, Item, OutputItem, OutputMessageContent, Reasoning, Response, ResponseStreamEvent,
    Role, Tool,
};
use async_trait::async_trait;
use futures_util::StreamExt;

use crate::{
    Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError, TokenUsage, ToolCall,
};

pub use async_openai::types::responses::ReasoningEffort;

mod transport;

#[cfg(test)]
mod tests;

/// A streaming provider using OpenAI's Responses API.
///
/// Enable the `openai` Cargo feature and use a Tokio runtime with I/O enabled.
/// The provider sends `store: false` and replays output items, including
/// encrypted reasoning, with subsequent tool results. Agent runs apply
/// [`RetryPolicy`](crate::RetryPolicy) to eligible failures before a stream opens.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use wakefold::{OpenAiProvider, ReasoningEffort, run};
///
/// let provider = OpenAiProvider::new(
///     "gpt-6-luna",
///     std::env::var("OPENAI_API_KEY")?,
/// )?
/// .with_reasoning_effort(ReasoningEffort::High);
/// let answer = run("Explain why the sky is blue.", &provider, &[], 1).await?;
/// println!("{answer}");
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct OpenAiProvider {
    client: Client<OpenAIConfig>,
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Create a provider for OpenAI with the model's default reasoning effort.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if the key cannot be used as a bearer token.
    pub fn new(
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.into();
        if !api_key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ProviderError::new("API key must be a single bearer token"));
        }
        let config = OpenAIConfig::new()
            .with_api_base(OPENAI_API_BASE)
            .with_api_key(api_key)
            .with_org_id("")
            .with_project_id("");
        Ok(Self {
            client: transport::client(config)?,
            model: model.into(),
            reasoning_effort: None,
        })
    }

    /// Request a reasoning effort supported by the selected model.
    ///
    /// GPT-6 Luna supports `None`, `Low`, `Medium`, `High`, `Xhigh`, and `Max`.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
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
        let tools = request.tools.iter().map(|spec| {
            Tool::Function(FunctionTool {
                name: spec.name.clone(),
                description: Some(spec.description.clone()),
                parameters: Some(spec.parameters.clone()),
                strict: Some(false),
                ..Default::default()
            })
        });
        let request = CreateResponse {
            model: Some(self.model.clone()),
            reasoning: self.reasoning_effort.clone().map(|effort| Reasoning {
                effort: Some(effort),
                ..Default::default()
            }),
            include: Some(vec![IncludeEnum::ReasoningEncryptedContent]),
            store: Some(false),
            input: InputParam::Items(input_items(request)?),
            tools: (!request.tools.is_empty()).then(|| tools.collect()),
            ..Default::default()
        };

        let mut stream = self
            .client
            .responses()
            .create_stream(request)
            .await
            .map_err(transport::opening_error)?;
        while let Some(event) = stream.next().await {
            match event
                .map_err(|source| ProviderError::with_source("Responses stream failed", source))?
            {
                ResponseStreamEvent::ResponseOutputTextDelta(event) if !event.delta.is_empty() => {
                    on_text(&event.delta);
                }
                ResponseStreamEvent::ResponseRefusalDelta(event) if !event.delta.is_empty() => {
                    on_text(&event.delta);
                }
                ResponseStreamEvent::ResponseCompleted(event) => {
                    return completed_turn(event.response);
                }
                ResponseStreamEvent::ResponseFailed(event) => {
                    let message = event.response.error.map_or_else(
                        || String::from("OpenAI response failed"),
                        |error| format!("OpenAI response failed: {}", error.message),
                    );
                    return Err(ProviderError::new(message));
                }
                ResponseStreamEvent::ResponseError(event) => {
                    return Err(ProviderError::new(event.message));
                }
                ResponseStreamEvent::ResponseIncomplete(event) => {
                    let message = event.response.incomplete_details.map_or_else(
                        || String::from("OpenAI response was incomplete"),
                        |details| format!("OpenAI response was incomplete: {}", details.reason),
                    );
                    return Err(ProviderError::new(message));
                }
                _ => {}
            }
        }

        Err(ProviderError::new(
            "Responses stream ended before completion",
        ))
    }
}

fn input_items(request: ModelRequest<'_>) -> Result<Vec<InputItem>, ProviderError> {
    let mut items = Vec::new();
    let mut context = request.assistant_context.iter();
    for message in request.messages {
        match message {
            Message::User(prompt) => items.push(text_message(Role::User, prompt)),
            Message::Assistant(response) => {
                if let Some(Some(context)) = context.next() {
                    let output: Vec<OutputItem> =
                        serde_json::from_value(context.clone()).map_err(|source| {
                            ProviderError::with_source("invalid Responses turn context", source)
                        })?;
                    items.extend(output.into_iter().map(InputItem::from));
                    continue;
                }

                if let Some(text) = response.text() {
                    items.push(text_message(Role::Assistant, text));
                }
                items.extend(response.tool_calls().iter().map(|call| {
                    Item::FunctionCall(FunctionToolCall {
                        call_id: call.id.to_string(),
                        name: call.name.clone(),
                        arguments: call.arguments.to_string(),
                        ..Default::default()
                    })
                    .into()
                }));
            }
            Message::Tool(result) => items.push(
                Item::FunctionCallOutput(FunctionCallOutputItemParam {
                    call_id: Some(result.call_id.to_string()),
                    output: FunctionCallOutput::Text(result.value.to_string()),
                    id: None,
                    status: None,
                    name: None,
                    namespace: None,
                    caller: None,
                })
                .into(),
            ),
        }
    }

    Ok(items)
}

fn text_message(role: Role, text: &str) -> InputItem {
    EasyInputMessage {
        role,
        content: EasyInputContent::Text(text.to_owned()),
        r#type: Default::default(),
        phase: None,
    }
    .into()
}

fn completed_turn(response: Response) -> Result<ModelTurn, ProviderError> {
    let mut calls = Vec::new();
    let mut text = String::new();
    for item in &response.output {
        match item {
            OutputItem::Message(message) => {
                for part in &message.content {
                    match part {
                        OutputMessageContent::OutputText(part) => text.push_str(&part.text),
                        OutputMessageContent::Refusal(part) => text.push_str(&part.refusal),
                    }
                }
            }
            OutputItem::FunctionCall(call) => {
                let arguments = serde_json::from_str(&call.arguments).map_err(|source| {
                    ProviderError::with_source(
                        format!("tool call {} contains invalid JSON", call.call_id),
                        source,
                    )
                })?;
                calls.push(ToolCall::new(
                    call.call_id.clone(),
                    call.name.clone(),
                    arguments,
                ));
            }
            OutputItem::Reasoning(_) => {}
            _ => {
                return Err(ProviderError::new(
                    "OpenAI returned an unsupported output item",
                ));
            }
        }
    }
    let result = if calls.is_empty() {
        ModelResponse::Final(text)
    } else {
        ModelResponse::ToolCalls {
            text: (!text.is_empty()).then_some(text),
            calls,
        }
    };
    let usage = response.usage.map(|usage| {
        TokenUsage::new(
            u64::from(usage.input_tokens),
            u64::from(usage.output_tokens),
        )
    });
    let mut turn = ModelTurn::new(result, usage);
    turn.context = Some(serde_json::to_value(response.output).map_err(|source| {
        ProviderError::with_source("could not preserve Responses turn context", source)
    })?);
    Ok(turn)
}
