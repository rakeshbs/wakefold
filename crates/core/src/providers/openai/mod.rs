//! OpenAI-compatible Chat Completions adapter.

mod client;
mod request;
mod stream;

#[cfg(test)]
mod tests;

pub use client::OpenAiProvider;
