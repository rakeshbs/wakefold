//! Read retry hints at the HTTP boundary before opening an event stream.

use std::time::{Duration, SystemTime};

use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::error::{ApiError, ApiErrorResponse, OpenAIError, WrappedError};
use async_openai::middleware::{HttpRequestFactory, ReqwestService};
use reqwest::header::RETRY_AFTER;
use reqwest::{Response, StatusCode};
use tower::{Service, ServiceExt, service_fn};

use crate::ProviderError;

pub(super) fn client(config: OpenAIConfig) -> Result<Client<OpenAIConfig>, ProviderError> {
    // The agent owns retries, so use a transport with a single HTTP attempt.
    let http = reqwest::Client::builder()
        .retry(reqwest::retry::never())
        .build()
        .map_err(|source| ProviderError::with_source("could not configure OpenAI HTTP", source))?;
    Ok(with_transport(
        Client::build(http.clone(), config),
        ReqwestService::new(http),
    ))
}

pub(super) fn with_transport<S>(client: Client<OpenAIConfig>, service: S) -> Client<OpenAIConfig>
where
    S: Service<HttpRequestFactory, Response = Response, Error = OpenAIError>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    client.with_http_service(service_fn(move |request| {
        let service = service.clone();
        async move {
            let response = service.oneshot(request).await?;
            classify_response(response)
                .await
                .map_err(|error| OpenAIError::Boxed(Box::new(error)))
        }
    }))
}

async fn classify_response(response: Response) -> Result<Response, ProviderError> {
    let status = response.status();
    let message = match status {
        StatusCode::TOO_MANY_REQUESTS => "OpenAI rate limit reached",
        StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => "OpenAI server error before streaming",
        _ => return Ok(response),
    };
    let retry_after = parse_retry_after(&response, SystemTime::now());
    let api_error = response.json::<WrappedError>().await.map_or_else(
        |_| ApiError {
            message: status.to_string(),
            ..Default::default()
        },
        |wrapped| wrapped.error,
    );
    let requires_action = [&api_error.code, &api_error.r#type]
        .into_iter()
        .any(|value| {
            matches!(
                value.as_deref(),
                Some("insufficient_quota" | "billing_hard_limit_reached" | "billing_not_active")
            )
        });
    let error = ProviderError::with_source(
        if requires_action {
            "OpenAI quota or billing error"
        } else {
            message
        },
        OpenAIError::ApiError(ApiErrorResponse {
            status_code: status,
            api_error,
        }),
    );
    Err(if requires_action {
        error
    } else if let Some(delay) = retry_after {
        error.retry_after(delay)
    } else {
        error.retry_with_backoff()
    })
}

fn parse_retry_after(response: &Response, now: SystemTime) -> Option<Duration> {
    let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or_default())
}

pub(super) fn opening_error(source: OpenAIError) -> ProviderError {
    const MESSAGE: &str = "could not open Responses stream";
    match source {
        OpenAIError::Boxed(source) => match source.downcast::<ProviderError>() {
            Ok(error) => *error,
            Err(source) => ProviderError::with_source(MESSAGE, OpenAIError::Boxed(source)),
        },
        OpenAIError::Reqwest(ref error) if error.is_connect() => {
            ProviderError::with_source(MESSAGE, source).retry_with_backoff()
        }
        source => ProviderError::with_source(MESSAGE, source),
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;

    #[test]
    fn accepts_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1_784_000_000);
        for (header, expected) in [
            (String::from("5"), Some(Duration::from_secs(5))),
            (
                httpdate::fmt_http_date(now + Duration::from_secs(7)),
                Some(Duration::from_secs(7)),
            ),
            (
                httpdate::fmt_http_date(now - Duration::from_secs(1)),
                Some(Duration::ZERO),
            ),
            (
                String::from("9999999999999999999999"),
                Some(Duration::from_secs(u64::MAX)),
            ),
            (String::from("not-a-delay"), None),
        ] {
            let response = http::Response::builder()
                .header(RETRY_AFTER, &header)
                .body(String::new())
                .unwrap()
                .into();
            assert_eq!(parse_retry_after(&response, now), expected, "{header}");
        }
    }
}
