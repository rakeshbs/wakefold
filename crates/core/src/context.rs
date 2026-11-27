//! Summarize older tool exchanges before model requests exceed the context budget.

use std::collections::HashSet;
use std::io::{self, Write};

use crate::{JsonValue, Message, ModelRequest, ModelResponse, ProviderError, RunError};

#[cfg(test)]
mod tests;

/// Mirrors the measured prefix of each transcript slice. Between compactions
/// the driver only appends, so a budget check measures just the new entries.
#[derive(Default)]
pub(crate) struct ContextSizes {
    messages: Vec<u128>,
    assistant_replay_data: Vec<u128>,
    total: u128,
}

pub(crate) struct Compaction<'a> {
    sizes: &'a mut ContextSizes,
    input: Vec<Message>,
    prefix: usize,
    transcript_end: usize,
    assistant_turns: usize,
    retained_messages: Vec<usize>,
    retained_context: Vec<usize>,
    retained_bytes: u128,
    limit: usize,
}

impl Compaction<'_> {
    pub(crate) fn request(&self) -> ModelRequest<'_> {
        ModelRequest::new(&self.input, &[])
    }

    pub(crate) fn apply(
        self,
        response: ModelResponse,
        messages: &mut Vec<Message>,
        assistant_replay_data: &mut Vec<Option<JsonValue>>,
    ) -> Result<(), RunError> {
        let ModelResponse::Final(summary) = response else {
            return Err(ProviderError::new("context compaction must return a text summary").into());
        };
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(ProviderError::new("context compaction returned an empty summary").into());
        }
        let required = self.retained_bytes + summary.len() as u128;
        if required > self.limit as u128 {
            return Err(RunError::ContextBudgetExceeded {
                limit_bytes: self.limit,
                required_bytes: required,
            });
        }

        let retained = self
            .retained_messages
            .iter()
            .map(|&index| messages[index].clone())
            .chain([Message::Summary(summary.to_owned())])
            .collect::<Vec<_>>();
        messages.splice(self.prefix..self.transcript_end, retained);
        let retained = self
            .retained_context
            .iter()
            .map(|&index| assistant_replay_data[index].clone())
            .collect::<Vec<_>>();
        assistant_replay_data.splice(..self.assistant_turns, retained);
        let retained_sizes = self
            .retained_messages
            .iter()
            .map(|&index| self.sizes.messages[index])
            .chain([summary.len() as u128])
            .collect::<Vec<_>>();
        self.sizes
            .messages
            .splice(self.prefix..self.transcript_end, retained_sizes);
        let retained_sizes = self
            .retained_context
            .iter()
            .map(|&index| self.sizes.assistant_replay_data[index])
            .collect::<Vec<_>>();
        self.sizes
            .assistant_replay_data
            .splice(..self.assistant_turns, retained_sizes);
        self.sizes.total = required;
        Ok(())
    }
}

impl ContextSizes {
    pub(crate) fn prepare(
        &mut self,
        messages: &[Message],
        assistant_replay_data: &[Option<JsonValue>],
        limit: usize,
    ) -> Result<Option<Compaction<'_>>, RunError> {
        for message in &messages[self.messages.len()..] {
            let size = message_bytes(message);
            self.messages.push(size);
            self.total += size;
        }
        for context in &assistant_replay_data[self.assistant_replay_data.len()..] {
            let size = context.as_ref().map_or(0, json_bytes);
            self.assistant_replay_data.push(size);
            self.total += size;
        }
        let budget = limit as u128;
        if self.total <= budget {
            return Ok(None);
        }

        // At a model-call boundary all tool calls are answered. Only consider
        // prefixes ending before an assistant message to keep each exchange whole.
        let latest = assistant_replay_data.len().saturating_sub(1);
        let prefix = if matches!(messages.first(), Some(Message::Developer(_))) {
            3
        } else {
            1
        };
        let newest_user = messages
            .iter()
            .rposition(|message| matches!(message, Message::User(_)));
        let mut input_bytes = self.messages[..prefix].iter().sum::<u128>();
        let mut retained_bytes = self.total;
        let mut assistant_turns = 0;
        let mut candidate = None;
        let mut loaded = HashSet::new();
        let mut keep_exchange = false;
        let mut retained_messages = Vec::new();
        let mut retained_context = Vec::new();
        for (index, message) in messages.iter().enumerate().skip(prefix) {
            if let Message::Assistant(response) = message {
                if assistant_turns > 0 && retained_bytes < budget {
                    let instruction = summary_instruction(budget - retained_bytes);
                    if input_bytes + instruction.len() as u128 <= budget {
                        candidate = Some((
                            index,
                            assistant_turns,
                            retained_bytes,
                            instruction,
                            retained_messages.len(),
                            retained_context.len(),
                        ));
                    }
                }
                if assistant_turns == latest {
                    break;
                }
                // Keep the first loader exchange for every skill, including all
                // calls in a mixed turn so the provider's call/result pairs stay intact.
                keep_exchange = false;
                for name in response
                    .tool_calls()
                    .iter()
                    .filter(|call| call.name == crate::skill::LOADER)
                    .filter_map(|call| call.arguments["name"].as_str())
                {
                    keep_exchange |= loaded.insert(name);
                }
                if keep_exchange {
                    retained_context.push(assistant_turns);
                } else {
                    retained_bytes -= self.assistant_replay_data[assistant_turns];
                }
                assistant_turns += 1;
            }
            let size = self.messages[index];
            input_bytes += size;
            if (keep_exchange && !matches!(message, Message::Summary(_)))
                || newest_user == Some(index)
            {
                retained_messages.push(index);
            } else {
                retained_bytes -= size;
            }
        }

        let Some((
            transcript_end,
            assistant_turns,
            retained_bytes,
            instruction,
            message_count,
            context_count,
        )) = candidate
        else {
            return Err(RunError::ContextBudgetExceeded {
                limit_bytes: limit,
                required_bytes: self.total,
            });
        };
        let mut input = messages[..transcript_end].to_vec();
        input.push(Message::User(instruction));
        retained_messages.truncate(message_count);
        retained_context.truncate(context_count);
        Ok(Some(Compaction {
            sizes: self,
            input,
            prefix,
            transcript_end,
            assistant_turns,
            retained_messages,
            retained_context,
            retained_bytes,
            limit,
        }))
    }
}

fn summary_instruction(max_bytes: u128) -> String {
    format!(
        "Summarize the earlier work for continuing the original task. Preserve relevant tool facts, decisions, and next steps. Return only the summary, up to {max_bytes} UTF-8 bytes."
    )
}

fn message_bytes(message: &Message) -> u128 {
    match message {
        Message::Developer(text) | Message::User(text) | Message::Summary(text) => {
            text.len() as u128
        }
        Message::Assistant(response) => {
            let text_bytes = response.text().map_or(0, str::len) as u128;
            response
                .tool_calls()
                .iter()
                .fold(text_bytes, |bytes, call| {
                    bytes
                        + call.id.as_str().len() as u128
                        + call.name.len() as u128
                        + json_bytes(&call.arguments)
                })
        }
        Message::Tool(result) => result.call_id.as_str().len() as u128 + json_bytes(&result.value),
    }
}

fn json_bytes(value: &JsonValue) -> u128 {
    // Counting bytes directly avoids allocating a copy of a large tool result.
    let mut counter = ByteCount(0);
    serde_json::to_writer(&mut counter, value).expect("JSON value serializes to a byte counter");
    counter.0
}

struct ByteCount(u128);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len() as u128;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
