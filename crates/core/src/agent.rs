//! In-memory agent decisions and the driver that executes them.

use std::collections::{BTreeMap, HashSet};

use thiserror::Error;

use crate::{
    CallId, Message, ModelRequest, Provider, ProviderError, Tool, ToolCall, ToolError, ToolResult,
};

/// The reason an agent run stopped before producing a final answer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunError {
    /// The provider could not supply the next model turn.
    #[error("model call failed: {0}")]
    Provider(#[from] ProviderError),

    /// Two tools advertised the same name.
    #[error("multiple tools registered as {name}")]
    DuplicateToolName {
        /// The repeated tool name.
        name: String,
    },

    /// The model requested a tool that was not registered.
    #[error("model requested unknown tool {name}")]
    UnknownTool {
        /// The requested tool name.
        name: String,
    },

    /// The provider reused an ID for a tool call already answered in the transcript.
    #[error("model repeated tool call ID {id}")]
    DuplicateCallId {
        /// The repeated call identifier.
        id: CallId,
    },

    /// A tool failed while executing the model's request.
    #[error("tool {name} failed: {source}")]
    Tool {
        /// The tool that failed.
        name: String,
        /// The failure reported by the tool.
        #[source]
        source: ToolError,
    },

    /// The run used all its model turns without a final answer.
    #[error("agent reached its model turn limit of {limit}")]
    TurnLimitReached {
        /// Maximum number of model calls allowed in the run.
        limit: usize,
    },
}

/// Run a single agent until it returns a final answer or reaches a limit.
///
/// If the last allowed model turn requests tools, the run stops before
/// executing them because no follow-up turn remains.
///
/// # Errors
///
/// Returns [`RunError`] if the provider or tool fails, the model requests an
/// invalid tool call, registration is ambiguous, or the turn limit is exhausted.
pub async fn run(
    prompt: impl Into<String> + Send,
    provider: &dyn Provider,
    tools: &[&dyn Tool],
    max_model_turns: usize,
) -> Result<String, RunError> {
    run_streaming(prompt, provider, tools, max_model_turns, |_| {}).await
}

/// Run an agent while delivering model text as it arrives.
///
/// The callback receives text from each turn, including any text accompanying
/// tool calls. Tools run after the provider returns a completed turn. The
/// returned string is the text of the final turn.
///
/// # Errors
///
/// Returns [`RunError`] under the same conditions as [`run`].
pub async fn run_streaming(
    prompt: impl Into<String> + Send,
    provider: &dyn Provider,
    tools: &[&dyn Tool],
    max_model_turns: usize,
    mut on_text: impl FnMut(&str) + Send,
) -> Result<String, RunError> {
    let specs: Vec<_> = tools.iter().map(|tool| tool.spec()).collect();
    let mut tools_by_name = BTreeMap::new();
    for (spec, &tool) in specs.iter().zip(tools) {
        if tools_by_name.insert(spec.name.as_str(), tool).is_some() {
            return Err(RunError::DuplicateToolName {
                name: spec.name.clone(),
            });
        }
    }

    let mut messages = vec![Message::User(prompt.into())];
    let mut remaining_turns = max_model_turns;
    loop {
        let action = next_action(&messages, remaining_turns).ok_or(RunError::TurnLimitReached {
            limit: max_model_turns,
        })?;
        match action {
            Action::CallModel => {
                let response = provider
                    .complete_streaming(
                        ModelRequest {
                            messages: &messages,
                            tools: &specs,
                        },
                        &mut on_text,
                    )
                    .await?;
                remaining_turns -= 1;
                if remaining_turns > 0 {
                    validate_calls(response.tool_calls(), &messages, &tools_by_name)?;
                }
                messages.push(Message::Assistant(response));
            }
            Action::RunTool(call) => {
                let tool = tools_by_name
                    .get(call.name.as_str())
                    .expect("tool names are validated before the turn is scheduled");
                let value = tool
                    .call(&call.arguments)
                    .await
                    .map_err(|source| RunError::Tool {
                        name: call.name.clone(),
                        source,
                    })?;
                let result = ToolResult::new(call.id.clone(), value);
                messages.push(Message::Tool(result));
            }
            Action::Finish(answer) => return Ok(answer.to_owned()),
        }
    }
}

enum Action<'a> {
    CallModel,
    RunTool(&'a ToolCall),
    Finish(&'a str),
}

fn next_action(messages: &[Message], remaining_turns: usize) -> Option<Action<'_>> {
    let completed_calls = messages
        .iter()
        .rev()
        .take_while(|message| matches!(message, Message::Tool(_)))
        .count();
    let turn = messages[..messages.len() - completed_calls].last()?;

    match turn {
        Message::Assistant(response) if response.tool_calls().is_empty() => {
            Some(Action::Finish(response.text().unwrap_or_default()))
        }
        _ if remaining_turns == 0 => None,
        Message::Assistant(response) => Some(
            response
                .tool_calls()
                .get(completed_calls)
                .map_or(Action::CallModel, Action::RunTool),
        ),
        Message::User(_) | Message::Tool(_) => Some(Action::CallModel),
    }
}

fn validate_calls(
    calls: &[ToolCall],
    messages: &[Message],
    tools: &BTreeMap<&str, &dyn Tool>,
) -> Result<(), RunError> {
    if calls.is_empty() {
        return Ok(());
    }
    let mut ids: HashSet<_> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Tool(result) => Some(&result.call_id),
            _ => None,
        })
        .collect();
    for call in calls {
        if !tools.contains_key(call.name.as_str()) {
            return Err(RunError::UnknownTool {
                name: call.name.clone(),
            });
        }
        if !ids.insert(&call.id) {
            return Err(RunError::DuplicateCallId {
                id: call.id.clone(),
            });
        }
    }
    Ok(())
}
