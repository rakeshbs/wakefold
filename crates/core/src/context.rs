//! Summarize older tool exchanges before model requests exceed the context budget.

use std::io::{self, Write};

use crate::{JsonValue, Message, ModelRequest, ModelResponse, ProviderError, RunError};

#[cfg(test)]
mod tests;

/// Mirrors the measured prefix of each transcript slice. Between compactions
/// the driver only appends, so a budget check measures just the new entries.
#[derive(Default)]
pub(crate) struct ContextSizes {
    messages: Vec<u128>,
    assistant_context: Vec<u128>,
    total: u128,
}

pub(crate) struct Compaction<'a> {
    sizes: &'a mut ContextSizes,
    input: Vec<Message>,
    transcript_end: usize,
    assistant_turns: usize,
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
        assistant_context: &mut Vec<Option<JsonValue>>,
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
                limit: self.limit,
                required,
            });
        }

        messages.splice(
            1..self.transcript_end,
            [Message::Summary(summary.to_owned())],
        );
        assistant_context.drain(..self.assistant_turns);
        self.sizes
            .messages
            .splice(1..self.transcript_end, [summary.len() as u128]);
        self.sizes.assistant_context.drain(..self.assistant_turns);
        self.sizes.total = required;
        Ok(())
    }
}

impl ContextSizes {
    pub(crate) fn prepare(
        &mut self,
        messages: &[Message],
        assistant_context: &[Option<JsonValue>],
        limit: usize,
    ) -> Result<Option<Compaction<'_>>, RunError> {
        for message in &messages[self.messages.len()..] {
            let size = message_bytes(message);
            self.messages.push(size);
            self.total += size;
        }
        for context in &assistant_context[self.assistant_context.len()..] {
            let size = context.as_ref().map_or(0, json_bytes);
            self.assistant_context.push(size);
            self.total += size;
        }
        let budget = limit as u128;
        if self.total <= budget {
            return Ok(None);
        }

        // At a model-call boundary all tool calls are answered. Only consider
        // prefixes ending before an assistant message to keep each exchange whole.
        let latest = assistant_context.len().saturating_sub(1);
        let mut input_bytes = self.messages.first().copied().unwrap_or(0);
        let mut retained_bytes = self.total;
        let mut assistant_turns = 0;
        let mut candidate = None;
        for (index, message) in messages.iter().enumerate().skip(1) {
            if matches!(message, Message::Assistant(_)) {
                if assistant_turns > 0 && retained_bytes < budget {
                    let instruction = summary_instruction(budget - retained_bytes);
                    if input_bytes + instruction.len() as u128 <= budget {
                        candidate = Some((index, assistant_turns, retained_bytes, instruction));
                    }
                }
                if assistant_turns == latest {
                    break;
                }
                retained_bytes -= self.assistant_context[assistant_turns];
                assistant_turns += 1;
            }
            let size = self.messages[index];
            input_bytes += size;
            retained_bytes -= size;
        }

        let Some((transcript_end, assistant_turns, retained_bytes, instruction)) = candidate else {
            return Err(RunError::ContextBudgetExceeded {
                limit,
                required: self.total,
            });
        };
        let mut input = messages[..transcript_end].to_vec();
        input.push(Message::User(instruction));
        Ok(Some(Compaction {
            sizes: self,
            input,
            transcript_end,
            assistant_turns,
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
        Message::User(text) | Message::Summary(text) => text.len() as u128,
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
