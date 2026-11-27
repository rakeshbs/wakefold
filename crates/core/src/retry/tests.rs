use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::time::{Instant as TestInstant, sleep};

use super::*;
use crate::{Message, ModelResponse, ProviderError, RunOptions, run_with_options};

struct Attempt {
    text: Option<&'static str>,
    response: Result<ModelResponse, ProviderError>,
}

struct Script(Mutex<VecDeque<Attempt>>);

impl Script {
    fn new(attempts: impl IntoIterator<Item = Attempt>) -> Self {
        Self(Mutex::new(attempts.into_iter().collect()))
    }

    fn remaining(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    async fn retry_turn(
        &self,
        policy: ModelRetryPolicy,
        cancellation: Option<&CancellationToken>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, RunError> {
        stream_turn_with_clock(
            self,
            (
                ModelRequest::new(&[Message::User(String::from("Hello"))], &[]),
                chrono_tz::UTC,
            ),
            on_text,
            policy,
            cancellation,
            || TestInstant::now().into_std(),
            sleep,
        )
        .await
    }
}

#[async_trait]
impl Provider for Script {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_response(request, &mut |_| {}).await
    }

    async fn stream_response(
        &self,
        request: ModelRequest<'_>,
        on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelResponse, ProviderError> {
        assert_eq!(request.messages, [Message::User(String::from("Hello"))]);
        let attempt = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected attempt");
        if let Some(text) = attempt.text {
            on_text(text);
        }
        attempt.response
    }
}

fn refusal(error: ProviderError) -> Attempt {
    Attempt {
        text: None,
        response: Err(error),
    }
}

fn final_answer() -> Attempt {
    Attempt {
        text: Some("Hi"),
        response: Ok(ModelResponse::Final(String::from("Hi"))),
    }
}

#[tokio::test(start_paused = true)]
async fn waits_for_server_delay_then_backs_off_when_no_delay_is_supplied() -> Result<(), RunError> {
    let provider = Script::new([
        refusal(ProviderError::new("busy").retry_after(Duration::from_secs(4))),
        refusal(ProviderError::new("busy").retry_with_backoff()),
        final_answer(),
    ]);
    let start = TestInstant::now();
    let mut text = String::new();
    let turn = provider
        .retry_turn(ModelRetryPolicy::default(), None, &mut |chunk| {
            text.push_str(chunk)
        })
        .await?;
    let elapsed = start.elapsed();
    assert!((Duration::from_millis(4_250)..=Duration::from_millis(4_500)).contains(&elapsed));
    assert_eq!(turn.response, ModelResponse::Final(String::from("Hi")));
    assert_eq!(text, "Hi");
    assert_eq!(provider.remaining(), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stops_when_attempts_run_out_or_the_server_delay_exceeds_the_window() {
    let provider = Script::new([
        refusal(ProviderError::new("first error").retry_after(Duration::ZERO)),
        refusal(ProviderError::new("last error").retry_after(Duration::ZERO)),
        final_answer(),
    ]);
    let error = provider
        .retry_turn(
            ModelRetryPolicy::new(1, Duration::from_secs(10)),
            None,
            &mut |_| {},
        )
        .await
        .expect_err("one retry is allowed");
    assert_eq!(error.to_string(), "model call failed: last error");
    assert_eq!(provider.remaining(), 1);

    let provider = Script::new([
        refusal(ProviderError::new("try later").retry_after(Duration::from_secs(10))),
        final_answer(),
    ]);
    let start = TestInstant::now();
    let error = provider
        .retry_turn(
            ModelRetryPolicy::new(3, Duration::from_secs(1)),
            None,
            &mut |_| {},
        )
        .await
        .expect_err("the server delay exceeds the time window");
    assert_eq!(error.to_string(), "model call failed: try later");
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(provider.remaining(), 1);
}

#[tokio::test(start_paused = true)]
async fn permanent_and_partial_stream_errors_surface_immediately() {
    let mut streamed = refusal(ProviderError::new("disconnected").retry_after(Duration::ZERO));
    streamed.text = Some("partial answer");
    for (attempt, expected_text) in [
        (refusal(ProviderError::new("invalid request")), ""),
        (streamed, "partial answer"),
    ] {
        let provider = Script::new([attempt, final_answer()]);
        let mut text = String::new();
        let result = provider
            .retry_turn(ModelRetryPolicy::default(), None, &mut |chunk| {
                text.push_str(chunk)
            })
            .await;
        assert!(matches!(result, Err(RunError::Provider(_))));
        assert_eq!(text, expected_text);
        assert_eq!(provider.remaining(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_interrupts_the_wait_before_another_attempt() {
    let provider = Script::new([
        refusal(ProviderError::new("busy").retry_after(Duration::from_secs(5))),
        final_answer(),
    ]);
    let cancellation = CancellationToken::new();
    let mut on_text = |_: &str| {};
    let (result, ()) = tokio::join!(
        provider.retry_turn(
            ModelRetryPolicy::default(),
            Some(&cancellation),
            &mut on_text
        ),
        async {
            sleep(Duration::from_secs(1)).await;
            cancellation.cancel();
        },
    );
    assert!(matches!(result, Err(RunError::Cancelled)));
    assert_eq!(provider.remaining(), 1);
}

#[test]
fn retries_a_model_turn_without_a_tokio_runtime_or_an_extra_turn() -> Result<(), RunError> {
    let provider = Script::new([
        refusal(ProviderError::new("busy").retry_after(Duration::from_millis(1))),
        final_answer(),
    ]);
    let report = futures_executor::block_on(run_with_options(
        "Hello",
        &provider,
        &[],
        RunOptions::new(1)
            .with_model_retry_policy(ModelRetryPolicy::new(1, Duration::from_secs(5))),
    ))?;
    assert_eq!(report.answer, "Hi");
    assert_eq!(provider.remaining(), 0);
    Ok(())
}
