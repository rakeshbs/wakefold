//! Translate Wakefold messages and tool specs into Chat Completions requests.

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionStreamOptions, ChatCompletionTool,
    ChatCompletionTools, CreateChatCompletionRequest, FunctionCall, FunctionObject,
};

use super::client::UsageReporting;
use crate::{Message, ModelRequest};

pub(super) fn chat_request(
    model: &str,
    request: ModelRequest<'_>,
    usage_reporting: UsageReporting,
) -> CreateChatCompletionRequest {
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
        model: model.to_owned(),
        messages: request.messages.iter().map(chat_message).collect(),
        tools: (!tools.is_empty()).then_some(tools),
        stream: Some(true),
        stream_options: (usage_reporting == UsageReporting::Enabled).then_some(
            ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            },
        ),
        ..Default::default()
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
