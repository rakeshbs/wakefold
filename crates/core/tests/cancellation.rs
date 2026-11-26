mod common;

use std::future::{Future, pending};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::testing::{ScriptedProvider, ScriptedTurn};
use wakefold::{
    CancellationToken, JsonValue, Message, ModelRequest, ModelResponse, Provider, ProviderError,
    RunError, RunOptions, Skill, Tool, ToolContext, ToolError, ToolSpec,
    run_streaming_with_options, run_with_options,
};

#[test]
fn cancellation_before_the_first_model_call() {
    let provider = ScriptedProvider::new([]);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let options = RunOptions::new(1).with_cancellation(cancellation);
    let result = block_on(run_with_options("Hello", &provider, &[], options));

    assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
}

struct MarkDropped<'a>(&'a AtomicBool);

impl Drop for MarkDropped<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct StreamingProvider {
    dropped: AtomicBool,
}

#[async_trait]
impl Provider for StreamingProvider {
    async fn complete_response(
        &self,
        _request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        Err(ProviderError::new("expected a streaming request"))
    }

    async fn stream_response(
        &self,
        _request: ModelRequest<'_>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        let _drop_guard = MarkDropped(&self.dropped);
        on_text("first");
        on_text("late");
        pending().await
    }
}

#[test]
fn cancellation_drops_the_model_stream_and_suppresses_late_text() {
    let provider = StreamingProvider::default();
    let cancellation = CancellationToken::new();
    let options = RunOptions::new(1).with_cancellation(cancellation.clone());
    let mut chunks = Vec::new();

    let result = {
        let mut run = pin!(run_streaming_with_options(
            "Hello",
            &provider,
            &[],
            options,
            |text| {
                chunks.push(text.to_owned());
                cancellation.cancel();
            },
        ));
        run.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    };

    assert!(
        matches!(result, Poll::Ready(Err(RunError::Cancelled))),
        "{result:?}"
    );
    assert_eq!(chunks, ["first"]);
    assert!(provider.dropped.load(Ordering::Relaxed));
}

#[derive(Default)]
struct PendingTool {
    started: AtomicBool,
    saw_cancellation: AtomicBool,
    dropped: AtomicBool,
}

#[async_trait]
impl Tool for PendingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("wait", "Wait for an event", json!({"type": "object"}))
    }

    async fn call(&self, _arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        Err(ToolError::new("expected a tool context"))
    }

    async fn call_with_context(
        &self,
        context: ToolContext,
        _arguments: &JsonValue,
    ) -> Result<JsonValue, ToolError> {
        let _drop_guard = MarkDropped(&self.dropped);
        let cancellation = context
            .cancellation_token()
            .ok_or_else(|| ToolError::new("expected the caller's cancellation signal"))?;

        self.started.store(true, Ordering::Relaxed);
        cancellation.cancelled().await;
        self.saw_cancellation.store(true, Ordering::Relaxed);
        pending().await
    }
}

fn scripted_tool_call(prompt: &str, name: &str, spec: ToolSpec) -> ScriptedProvider {
    let (load, messages) = common::load_skill(&Message::User(prompt.to_owned()), "util", &[spec]);
    ScriptedProvider::new([
        load,
        ScriptedTurn::new(
            messages,
            ModelResponse::ToolCall(common::call_tool("call-1", name, json!({}))),
        ),
    ])
}

#[test]
fn in_flight_tool_can_observe_cancellation_before_it_is_dropped() {
    let prompt = "Wait for an event";
    let provider = scripted_tool_call(prompt, "util.wait", PendingTool::default().spec());
    let tool = PendingTool::default();
    let tools: [&dyn Tool; 1] = [&tool];
    let skills = [Skill::new("util", "Utility tools", &tools)];
    let cancellation = CancellationToken::new();
    let options = RunOptions::new(3).with_cancellation(cancellation.clone());
    let mut run = pin!(run_with_options(prompt, &provider, &skills, options));
    let mut context = Context::from_waker(Waker::noop());

    assert!(run.as_mut().poll(&mut context).is_pending());
    assert!(tool.started.load(Ordering::Relaxed));

    cancellation.cancel();
    let result = run.as_mut().poll(&mut context);
    assert!(
        matches!(result, Poll::Ready(Err(RunError::Cancelled))),
        "{result:?}"
    );
    assert!(tool.saw_cancellation.load(Ordering::Relaxed));
    assert!(tool.dropped.load(Ordering::Relaxed));
}

#[cfg(feature = "derive")]
#[test]
fn typed_tool_receives_the_run_cancellation_signal() {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use wakefold::{WakefoldTool, typed_tool_with_context};

    #[derive(Deserialize, JsonSchema, WakefoldTool)]
    #[tool(name = "stop", description = "Stop the current run")]
    struct Stop {}

    let prompt = "Stop the current run";
    let cancellation = CancellationToken::new();
    let options = RunOptions::new(3).with_cancellation(cancellation.clone());
    let tool = typed_tool_with_context(|_input: Stop, context| async move {
        context
            .cancellation_token()
            .ok_or_else(|| ToolError::new("expected the caller's cancellation signal"))?
            .cancel();
        Ok(json!({}))
    });

    let provider = scripted_tool_call(prompt, "util.stop", tool.spec());
    let result = block_on(run_with_options(
        prompt,
        &provider,
        &[Skill::new("util", "Utility tools", &[&tool])],
        options,
    ));

    assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
    assert!(cancellation.is_cancelled());
}
