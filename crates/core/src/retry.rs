//! Retry model requests with provider-supplied timing and bounded backoff.

use std::future::Future;
use std::time::{Duration, Instant};

use futures_timer::Delay;

use crate::agent::await_cancellable;
use crate::provider::RetryAdvice;
use crate::{CancellationToken, ModelRequest, ModelTurn, Provider, RunError};

#[cfg(test)]
mod tests;

/// Limits for retrying a model turn after a transient provider failure.
///
/// By default, a turn can be retried up to three times within 30 seconds of
/// its first attempt. Once an attempt starts, it can run to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_retries: u32,
    retry_window: Duration,
}

impl RetryPolicy {
    /// Set the maximum additional attempts and the time window for starting them.
    ///
    /// Either limit can be set to zero to disable retries.
    pub fn new(max_retries: u32, retry_window: Duration) -> Self {
        Self {
            max_retries,
            retry_window,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(30))
    }
}

pub(crate) async fn stream_turn(
    provider: &dyn Provider,
    request: ModelRequest<'_>,
    on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    policy: RetryPolicy,
    cancellation: Option<&CancellationToken>,
) -> Result<ModelTurn, RunError> {
    stream_turn_with_clock(
        provider,
        request,
        on_text,
        policy,
        cancellation,
        Instant::now,
        Delay::new,
    )
    .await
}

async fn stream_turn_with_clock<Sleep: Future<Output = ()>>(
    provider: &dyn Provider,
    request: ModelRequest<'_>,
    on_text: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    policy: RetryPolicy,
    cancellation: Option<&CancellationToken>,
    now: impl Fn() -> Instant,
    sleep: impl Fn(Duration) -> Sleep,
) -> Result<ModelTurn, RunError> {
    let started = now();
    let mut retries = 0;
    loop {
        let mut streamed = false;
        let result = await_cancellable(
            provider.stream_turn(request, &mut |chunk| {
                streamed = true;
                on_text(chunk);
            }),
            cancellation,
        )
        .await?;
        let error = match result {
            Ok(turn) => return Ok(turn),
            Err(error) if streamed || retries >= policy.max_retries => return Err(error.into()),
            Err(error) => error,
        };

        let delay = match error.retry_advice() {
            RetryAdvice::Never => return Err(error.into()),
            RetryAdvice::Backoff => backoff(retries),
            RetryAdvice::After(delay) => delay,
        };
        let failed_at = now();
        let remaining = policy
            .retry_window
            .saturating_sub(failed_at.saturating_duration_since(started));
        if delay >= remaining || failed_at.checked_add(delay).is_none() {
            return Err(error.into());
        }

        if !delay.is_zero() {
            await_cancellable(sleep(delay), cancellation).await?;
        }
        if now().saturating_duration_since(started) >= policy.retry_window {
            return Err(error.into());
        }
        retries += 1;
    }
}

fn backoff(retries: u32) -> Duration {
    const INITIAL_BACKOFF_MS: u64 = 250;
    const MAX_BACKOFF_MS: u64 = 8_000;
    let ceiling = (INITIAL_BACKOFF_MS << retries.min(5)).min(MAX_BACKOFF_MS);
    Duration::from_millis(rand::random_range(ceiling / 2..=ceiling))
}
