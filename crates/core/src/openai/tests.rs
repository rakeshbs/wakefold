use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_openai::error::OpenAIError;
use async_openai::middleware::HttpRequestFactory;
use async_trait::async_trait;
use http::Response;
use serde_json::json;
use tower::service_fn;

use super::OpenAiProvider;
use crate::{
    JsonValue, Message, ModelRequest, ModelResponse, Provider, ProviderError, RunError, Tool,
    ToolError, ToolSpec, run_streaming,
};

struct Exchange {
    messages: JsonValue,
    tools: Option<JsonValue>,
    chunks: Vec<JsonValue>,
}

fn offline_provider(exchanges: Vec<Exchange>) -> Result<OpenAiProvider, ProviderError> {
    let exchanges = Arc::new(Mutex::new(VecDeque::from(exchanges)));
    let service = service_fn(move |factory: HttpRequestFactory| {
        let exchanges = Arc::clone(&exchanges);
        async move {
            let exchange = exchanges
                .lock()
                .expect("only this test accesses its canned responses")
                .pop_front()
                .expect("the provider must make only the scripted requests");
            let request = factory.build().await?;
            assert_eq!(
                request.url().as_str(),
                "http://example.test/v1/chat/completions"
            );
            assert_eq!(request.method(), "POST");
            assert_eq!(request.headers()["authorization"], "Bearer offline-key");
            let bytes = request
                .body()
                .and_then(|body| body.as_bytes())
                .expect("a chat request has a JSON body");
            let body: JsonValue =
                serde_json::from_slice(bytes).expect("the SDK must serialize valid JSON");
            assert_eq!(body["stream"], true);
            assert_eq!(body["model"], "offline-model");
            assert_eq!(body["messages"], exchange.messages);
            assert_eq!(body.get("tools"), exchange.tools.as_ref());

            let mut sse = String::new();
            for chunk in exchange.chunks {
                sse.push_str(&format!("data: {chunk}\n\n"));
            }
            sse.push_str("data: [DONE]\n\n");

            Ok::<_, OpenAIError>(
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(sse)
                    .expect("the canned SSE response is valid")
                    .into(),
            )
        }
    });

    let mut provider =
        OpenAiProvider::with_base_url("offline-model", "http://example.test/v1/", "offline-key")?;
    provider.client = provider.client.with_http_service(service);
    Ok(provider)
}

fn chunk(delta: JsonValue, finish_reason: Option<&str>) -> JsonValue {
    json!({
        "id": "chatcmpl-offline",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "offline-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
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

fn advertised_tools() -> JsonValue {
    json!([{
        "type": "function",
        "function": {
            "name": "lookup_price",
            "description": "Look up an item price",
            "parameters": catalog_schema()
        }
    }])
}

#[derive(Default)]
struct LookupPrice {
    items: Mutex<Vec<String>>,
}

#[async_trait]
impl Tool for LookupPrice {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("lookup_price", "Look up an item price", catalog_schema())
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        let item = arguments["item"]
            .as_str()
            .ok_or_else(|| ToolError::new("expected an item name"))?;
        self.items
            .lock()
            .expect("only this test invokes the tool")
            .push(item.to_owned());
        let price = match item {
            "notebook" => 350,
            "pen" => 120,
            _ => return Err(ToolError::new("unknown item")),
        };
        Ok(json!({"price_cents": price}))
    }
}

#[tokio::test]
async fn streams_text_and_round_trips_interleaved_tool_calls()
-> Result<(), Box<dyn std::error::Error>> {
    let prompt = "What do a notebook and a pen cost?";
    let user = json!({"role": "user", "content": prompt});
    let tools = advertised_tools();
    let provider = offline_provider(vec![
        Exchange {
            messages: json!([user]),
            tools: Some(tools.clone()),
            chunks: vec![
                chunk(
                    json!({
                        "content": "Checking. ",
                        "tool_calls": [{
                            "index": 1, "id": "price-pen", "type": "function",
                            "function": {"name": "lookup_price", "arguments": "{\"item\":"}
                        }]
                    }),
                    None,
                ),
                chunk(
                    json!({
                        "tool_calls": [
                            {
                                "index": 0, "id": "price-notebook", "type": "function",
                                "function": {"name": "lookup_price", "arguments": "{\"item\":\"note"}
                            },
                            {"index": 1, "function": {"arguments": "\"pen\"}"}}
                        ]
                    }),
                    None,
                ),
                chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "book\"}"}}]}),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ],
        },
        Exchange {
            messages: json!([
                user,
                {
                    "role": "assistant",
                    "content": "Checking. ",
                    "tool_calls": [
                        {
                            "id": "price-notebook",
                            "type": "function",
                            "function": {"name": "lookup_price", "arguments": "{\"item\":\"notebook\"}"}
                        },
                        {
                            "id": "price-pen",
                            "type": "function",
                            "function": {"name": "lookup_price", "arguments": "{\"item\":\"pen\"}"}
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "price-notebook", "content": "{\"price_cents\":350}"},
                {"role": "tool", "tool_call_id": "price-pen", "content": "{\"price_cents\":120}"}
            ]),
            tools: Some(tools),
            chunks: vec![
                chunk(json!({"content": "Notebook: $3.50. "}), None),
                chunk(json!({"content": "Pen: $1.20."}), None),
                chunk(json!({}), Some("stop")),
            ],
        },
    ])?;
    let tool = LookupPrice::default();
    let mut chunks = Vec::new();

    let answer = run_streaming(prompt, &provider, &[&tool], 2, |text| {
        chunks.push(text.to_owned());
    })
    .await?;

    assert_eq!(answer, "Notebook: $3.50. Pen: $1.20.");
    assert_eq!(chunks, ["Checking. ", "Notebook: $3.50. ", "Pen: $1.20."]);
    assert_eq!(
        *tool.items.lock().expect("the tool call completed"),
        ["notebook", "pen"]
    );
    Ok(())
}

#[tokio::test]
async fn completes_a_plain_text_request_without_advertising_tools() -> Result<(), ProviderError> {
    let provider = offline_provider(vec![Exchange {
        messages: json!([{"role": "user", "content": "Hello"}]),
        tools: None,
        chunks: vec![
            chunk(json!({"content": "Hello"}), None),
            chunk(json!({"content": " there."}), Some("stop")),
        ],
    }])?;

    let response = provider
        .complete(ModelRequest {
            messages: &[Message::User(String::from("Hello"))],
            tools: &[],
        })
        .await?;

    assert_eq!(response, ModelResponse::Final(String::from("Hello there.")));
    Ok(())
}

#[tokio::test]
async fn interrupted_or_invalid_calls_do_not_dispatch_tools() -> Result<(), ProviderError> {
    let prompt = "What does a notebook cost?";
    for (arguments, finish, expected_error) in [
        (
            "{\"item\":\"notebook\"}",
            None,
            "ended before a finish reason",
        ),
        ("{\"item\":\"notebook\"}", Some("length"), "token limit"),
        ("{\"item\":", Some("tool_calls"), "invalid JSON"),
    ] {
        let provider = offline_provider(vec![Exchange {
            messages: json!([{"role": "user", "content": prompt}]),
            tools: Some(advertised_tools()),
            chunks: vec![chunk(
                json!({
                    "tool_calls": [{
                        "index": 0, "id": "price-1", "type": "function",
                        "function": {"name": "lookup_price", "arguments": arguments}
                    }]
                }),
                finish,
            )],
        }])?;
        let tool = LookupPrice::default();

        let result = run_streaming(prompt, &provider, &[&tool], 2, |_| {}).await;

        assert!(
            matches!(&result, Err(RunError::Provider(error)) if error.to_string().contains(expected_error)),
            "{result:?}"
        );
        assert!(
            tool.items
                .lock()
                .expect("the tool was never called")
                .is_empty()
        );
    }
    Ok(())
}
