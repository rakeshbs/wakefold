use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_openai::error::OpenAIError;
use async_openai::middleware::HttpRequestFactory;
use http::Response as HttpResponse;
use serde_json::json;
use tower::service_fn;

use super::*;
use crate::{
    JsonValue, RetryPolicy, RunError, RunOptions, Skill, Tool as WakefoldTool, ToolError, ToolSpec,
    run_streaming_with_options,
};

struct Exchange {
    input: JsonValue,
    tools: Option<JsonValue>,
    events: Vec<JsonValue>,
}

fn offline_provider(
    effort: Option<(ReasoningEffort, &'static str)>,
    exchanges: Vec<Exchange>,
) -> Result<OpenAiProvider, ProviderError> {
    let exchanges = Arc::new(Mutex::new(VecDeque::from(exchanges)));
    let wire_effort = effort.as_ref().map(|(_, wire_effort)| *wire_effort);
    let service = service_fn(move |factory: HttpRequestFactory| {
        let exchanges = Arc::clone(&exchanges);
        async move {
            let exchange = exchanges
                .lock()
                .expect("the test exchanges are available")
                .pop_front()
                .expect("the provider must make only the scripted requests");
            let request = factory.build().await?;
            assert_eq!(
                request.url().as_str(),
                "https://api.openai.com/v1/responses"
            );
            assert_eq!(request.method(), "POST");
            assert_eq!(request.headers()["authorization"], "Bearer offline-key");
            let bytes = request.body().and_then(|body| body.as_bytes()).unwrap();
            let body: JsonValue = serde_json::from_slice(bytes).unwrap();
            assert_eq!(body["model"], "gpt-6-luna");
            assert_eq!(body["stream"], true);
            let reasoning = wire_effort.map(|value| json!({"effort": value}));
            assert_eq!(body.get("reasoning"), reasoning.as_ref());
            assert_eq!(body["store"], false);
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
            assert_eq!(body.get("previous_response_id"), None);
            assert_eq!(body["input"], exchange.input);
            assert_eq!(body.get("tools"), exchange.tools.as_ref());

            Ok::<_, OpenAIError>(stream_response(exchange.events).into())
        }
    });

    let mut provider = OpenAiProvider::new("gpt-6-luna", "offline-key")?;
    if let Some((effort, _)) = effort {
        provider = provider.with_reasoning_effort(effort);
    }
    provider.client = transport::with_transport(provider.client, service);
    Ok(provider)
}

fn stream_response(events: Vec<JsonValue>) -> HttpResponse<String> {
    let body = events
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    HttpResponse::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap()
}

fn terminal_event(status: &str, output: Vec<JsonValue>) -> JsonValue {
    json!({
        "type": format!("response.{status}"),
        "sequence_number": 2,
        "response": {
            "id": "resp_offline",
            "object": "response",
            "created_at": 0,
            "model": "gpt-6-luna",
            "status": status,
            "output": output,
            "usage": {
                "input_tokens": 20,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 6,
                "output_tokens_details": {"reasoning_tokens": 2},
                "total_tokens": 26
            }
        }
    })
}

fn text_delta(delta: &str, sequence: u64) -> JsonValue {
    json!({
        "type": "response.output_text.delta",
        "sequence_number": sequence,
        "item_id": "msg_answer",
        "output_index": 0,
        "content_index": 0,
        "delta": delta
    })
}

fn catalog_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {"item": {"type": "string"}},
        "required": ["item"],
        "additionalProperties": false
    })
}

fn loader_tools() -> JsonValue {
    json!([
        {
            "type": "function", "name": "load_skill",
            "description": "Load a skill by name to receive its tool definitions. Use call_tool with a returned tool name and arguments on subsequent turns. Available skills:\ncatalog: Look up prices",
            "parameters": {
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"], "additionalProperties": false
            },
            "strict": false
        },
        {
            "type": "function", "name": "call_tool",
            "description": "Execute a tool from a loaded skill. Supply its qualified name and arguments matching the schema returned by load_skill.",
            "parameters": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "arguments": {}},
                "required": ["name", "arguments"], "additionalProperties": false
            },
            "strict": false
        }
    ])
}

fn load_call() -> JsonValue {
    json!({
        "type": "function_call", "name": "load_skill",
        "id": "fc_load", "call_id": "load_catalog", "arguments": r#"{"name":"catalog"}"#,
        "status": "completed"
    })
}

fn loaded_transcript() -> Vec<JsonValue> {
    vec![
        prompt(),
        load_call(),
        json!({
            "type": "function_call_output", "call_id": "load_catalog",
            "output": json!({"loaded": "catalog", "tools": [{
                "name": "catalog.lookup_price", "description": "Look up an item price",
                "parameters": catalog_schema()
            }]}).to_string()
        }),
    ]
}

fn output_message(text: &str) -> JsonValue {
    json!({
        "type": "message",
        "id": "msg_answer",
        "role": "assistant",
        "phase": "final_answer",
        "status": "completed",
        "content": [{
            "type": "output_text",
            "annotations": [],
            "text": text
        }]
    })
}

#[derive(Default)]
struct LookupPrice(Mutex<Vec<String>>);

#[async_trait]
impl WakefoldTool for LookupPrice {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("lookup_price", "Look up an item price", catalog_schema())
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        let item = arguments["item"].as_str().unwrap();
        self.0.lock().unwrap().push(item.to_owned());
        Ok(json!({"price_cents": 350}))
    }
}

const PROMPT: &str = "How much is a notebook?";

fn prompt() -> JsonValue {
    json!({"type": "message", "role": "user", "content": PROMPT})
}

fn call() -> JsonValue {
    json!({
        "type": "function_call",
        "id": "fc_offline",
        "call_id": "call_notebook",
        "name": "call_tool",
        "arguments": json!({"name": "catalog.lookup_price", "arguments": {"item": "notebook"}}).to_string(),
        "status": "completed"
    })
}

#[tokio::test]
async fn streams_function_calls_and_replays_reasoning_at_every_effort() -> Result<(), RunError> {
    for (effort, wire_effort) in [
        (ReasoningEffort::None, "none"),
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::Xhigh, "xhigh"),
        (ReasoningEffort::Max, "max"),
    ] {
        let mut output = Vec::new();
        if effort != ReasoningEffort::None {
            output.push(json!({
                "type": "reasoning",
                "id": "rs_offline",
                "summary": [],
                "encrypted_content": "opaque-encrypted-reasoning"
            }));
        }
        output.push(call());
        let mut followup = loaded_transcript();
        followup.extend(output.iter().cloned());
        followup.push(json!({
            "type": "function_call_output",
            "call_id": "call_notebook",
            "output": "{\"price_cents\":350}"
        }));
        let provider = offline_provider(
            Some((effort, wire_effort)),
            vec![
                Exchange {
                    input: json!([prompt()]),
                    tools: Some(loader_tools()),
                    events: vec![terminal_event("completed", vec![load_call()])],
                },
                Exchange {
                    input: json!(loaded_transcript()),
                    tools: Some(loader_tools()),
                    events: vec![terminal_event("completed", output)],
                },
                Exchange {
                    input: json!(followup),
                    tools: Some(loader_tools()),
                    events: vec![
                        text_delta("A notebook ", 0),
                        text_delta("costs $3.50.", 1),
                        terminal_event(
                            "completed",
                            vec![output_message("A notebook costs $3.50.")],
                        ),
                    ],
                },
            ],
        )?;
        let tool = LookupPrice::default();
        let mut chunks = Vec::new();
        let report = run_streaming_with_options(
            PROMPT,
            &provider,
            &[Skill::new("catalog", "Look up prices", &[&tool])],
            RunOptions::new(3).with_token_budget(100),
            |text| chunks.push(text.to_owned()),
        )
        .await?;

        assert_eq!(*tool.0.lock().unwrap(), ["notebook"]);
        assert_eq!(chunks, ["A notebook ", "costs $3.50."]);
        assert_eq!(report.answer, "A notebook costs $3.50.");
        assert_eq!(report.usage.reported, TokenUsage::new(60, 18));
        assert_eq!(report.usage.unreported_turns, 0);
    }

    Ok(())
}

#[tokio::test]
async fn rejects_incomplete_turns_and_invalid_arguments_before_dispatch()
-> Result<(), ProviderError> {
    let mut incomplete = terminal_event("incomplete", vec![call()]);
    incomplete["response"]["incomplete_details"] = json!({"reason": "max_output_tokens"});
    let mut invalid_call = call();
    invalid_call["arguments"] = json!("{");
    for (events, expected_error) in [
        (vec![incomplete], "incomplete"),
        (
            vec![terminal_event("completed", vec![invalid_call])],
            "invalid JSON",
        ),
        (vec![], "ended before completion"),
    ] {
        let provider = offline_provider(
            Some((ReasoningEffort::High, "high")),
            vec![Exchange {
                input: json!([prompt()]),
                tools: Some(loader_tools()),
                events,
            }],
        )?;
        let tool = LookupPrice::default();
        let result = run_streaming_with_options(
            PROMPT,
            &provider,
            &[Skill::new("catalog", "Look up prices", &[&tool])],
            RunOptions::new(3),
            |_| {},
        )
        .await;

        let Err(RunError::Provider(error)) = result else {
            panic!("expected a provider error, got {result:?}");
        };
        assert!(error.to_string().contains(expected_error), "{error}");
        assert!(tool.0.lock().unwrap().is_empty());
    }

    Ok(())
}

#[tokio::test]
async fn uses_model_defaults_for_text_only_requests() -> Result<(), ProviderError> {
    let mut completed = terminal_event("completed", vec![output_message("Hello there.")]);
    completed["response"]
        .as_object_mut()
        .unwrap()
        .remove("usage");
    let provider = offline_provider(
        None,
        vec![Exchange {
            input: json!([{"type": "message", "role": "user", "content": "Hello"}]),
            tools: None,
            events: vec![text_delta("Hello there.", 0), completed],
        }],
    )?;

    let turn = provider
        .complete_turn(
            ModelRequest::new(&[Message::User(String::from("Hello"))], &[]),
            &mut |_| {},
        )
        .await?;

    assert_eq!(
        turn.response,
        ModelResponse::Final(String::from("Hello there."))
    );
    assert_eq!(turn.usage, None);
    Ok(())
}

struct RetryFixture {
    provider: OpenAiProvider,
    responses: Arc<Mutex<VecDeque<HttpResponse<String>>>>,
}

fn retry_provider(responses: Vec<HttpResponse<String>>) -> Result<RetryFixture, ProviderError> {
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let pending = Arc::clone(&responses);
    let service = service_fn(move |_request: HttpRequestFactory| {
        let response = pending
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected HTTP attempt");
        std::future::ready(Ok::<_, OpenAIError>(response.into()))
    });
    let mut provider = OpenAiProvider::new("gpt-6-luna", "offline-key")?;
    provider.client = transport::with_transport(provider.client, service);
    Ok(RetryFixture {
        provider,
        responses,
    })
}

fn rate_limited(code: &str, retry_after: &str) -> HttpResponse<String> {
    HttpResponse::builder()
        .status(429)
        .header("retry-after", retry_after)
        .header("content-type", "application/json")
        .body(json!({"error": {"message": "limit reached", "code": code}}).to_string())
        .unwrap()
}

#[tokio::test]
async fn retries_a_rate_limited_follow_up_without_replaying_the_tool() -> Result<(), RunError> {
    let RetryFixture {
        provider,
        responses,
    } = retry_provider(vec![
        stream_response(vec![terminal_event("completed", vec![load_call()])]),
        stream_response(vec![terminal_event("completed", vec![call()])]),
        rate_limited("rate_limit_exceeded", "0"),
        stream_response(vec![
            text_delta("$3.50", 0),
            terminal_event("completed", vec![output_message("$3.50")]),
        ]),
    ])?;
    let tool = LookupPrice::default();
    let mut streamed = String::new();
    let report = run_streaming_with_options(
        PROMPT,
        &provider,
        &[Skill::new("catalog", "Look up prices", &[&tool])],
        RunOptions::new(3),
        |text| streamed.push_str(text),
    )
    .await?;
    assert_eq!(report.answer, "$3.50");
    assert_eq!(streamed, "$3.50");
    assert_eq!(*tool.0.lock().unwrap(), ["notebook"]);
    assert_eq!(report.usage.reported, TokenUsage::new(60, 18));
    assert!(responses.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn preserves_retry_after_and_stops_on_quota_errors() -> Result<(), ProviderError> {
    for (rejection, expected_message) in [
        (
            rate_limited("rate_limit_exceeded", "120"),
            "OpenAI rate limit reached",
        ),
        (
            HttpResponse::builder()
                .status(503)
                .header("retry-after", "120")
                .body(String::from("Service unavailable"))
                .unwrap(),
            "OpenAI server error before streaming",
        ),
        (
            rate_limited("insufficient_quota", "0"),
            "OpenAI quota or billing error",
        ),
    ] {
        let RetryFixture {
            provider,
            responses,
        } = retry_provider(vec![
            rejection,
            stream_response(vec![terminal_event(
                "completed",
                vec![output_message("Hi")],
            )]),
        ])?;
        let error = run_streaming_with_options(
            "Hello",
            &provider,
            &[],
            RunOptions::new(1).with_retry_policy(RetryPolicy::new(1, Duration::from_secs(2))),
            |_| {},
        )
        .await
        .expect_err("no retry fits this request");
        let RunError::Provider(error) = error else {
            panic!("expected a provider failure, got {error:?}");
        };
        assert_eq!(error.to_string(), expected_message);
        assert_eq!(responses.lock().unwrap().len(), 1);
    }
    Ok(())
}
