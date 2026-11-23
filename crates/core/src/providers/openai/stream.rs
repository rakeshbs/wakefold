//! Assemble streamed Chat Completions text, tool calls, and usage.

use std::collections::BTreeMap;

use async_openai::types::chat::{
    ChatCompletionMessageToolCallChunk, ChatCompletionResponseStream, FinishReason,
};
use futures_util::StreamExt;

use super::client::UsageReporting;
use crate::{ModelResponse, ModelTurn, ProviderError, TokenUsage, ToolCall};

pub(super) async fn collect_turn(
    mut stream: ChatCompletionResponseStream,
    usage_reporting: UsageReporting,
    on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
) -> Result<ModelTurn, ProviderError> {
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
        if response.is_some() && (usage_reporting == UsageReporting::Disabled || usage.is_some()) {
            break;
        }
    }

    let response =
        response.ok_or_else(|| ProviderError::new("chat stream ended before a finish reason"))?;
    Ok(ModelTurn::new(response, usage))
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
