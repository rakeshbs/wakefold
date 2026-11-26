use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use wakefold::{
    CancellationToken, Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError,
    TokenUsage, Tool, ToolCall, ToolError, ToolSpec,
};

use crate::config::{Config, RegisteredSkill, Runtime};
use crate::{DaemonError, server};

const CHILD_CONFIG: &str = "WAKEFOLD_DAEMON_TEST_CONFIG";
const PROMPT: &str = "Find the notebook price and record an approved review.";
const FINAL_ANSWER: &str = "The notebook costs 350 cents and its review was recorded.";
type EventSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct OrderProvider;

fn dispatch(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall::new(
        id,
        "call_tool",
        json!({ "tool_name": name, "arguments": arguments }),
    )
}

#[async_trait]
impl Provider for OrderProvider {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver requests token usage with each turn")
    }

    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let Some(Message::User(prompt)) = request.messages.first() else {
            panic!("the original prompt must survive");
        };
        let skill = if prompt == PROMPT {
            "delegate"
        } else {
            "orders"
        };
        let response = match request.messages.last().unwrap() {
            Message::User(_) => {
                ToolCall::new("load", "load_skill", json!({ "name": skill })).into()
            }
            Message::Tool(result) if result.call_id == "load".into() => match prompt.as_str() {
                PROMPT => ModelResponse::ToolCalls {
                    text: None,
                    calls: vec![
                        dispatch(
                            "catalog",
                            "delegate.catalog",
                            json!({ "task": "Read the price." }),
                        ),
                        dispatch(
                            "review",
                            "delegate.review",
                            json!({ "task": "Record the review." }),
                        ),
                    ],
                },
                "Read the price." => {
                    dispatch("work", "orders.lookup", json!({ "sku": "WF-318" })).into()
                }
                "Record the review." => {
                    dispatch("work", "orders.review", json!({ "sku": "WF-318" })).into()
                }
                other => panic!("unexpected prompt {other}"),
            },
            Message::Tool(result) if prompt == PROMPT => {
                assert_eq!(result.call_id, "review".into());
                assert_eq!(
                    result.value,
                    json!({ "worker": "reviewer", "answer": "reviewed" })
                );
                let Message::Tool(catalog) = &request.messages[request.messages.len() - 2] else {
                    panic!("the catalog result must survive");
                };
                assert_eq!(
                    catalog.value,
                    json!({ "worker": "catalog", "answer": "350 cents" })
                );
                ModelResponse::Final(FINAL_ANSWER.into())
            }
            Message::Tool(result) => {
                assert_eq!(result.call_id, "work".into());
                ModelResponse::Final(result.value.as_str().unwrap().into())
            }
            other => panic!("unexpected message {other:?}"),
        };
        Ok(ModelTurn::new(response, Some(TokenUsage::new(2, 1))))
    }
}

struct OrderTool {
    state: PathBuf,
    name: &'static str,
    answer: &'static str,
}

#[async_trait]
impl Tool for OrderTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name, "Process the order", json!({"type": "object"}))
    }

    async fn call(&self, args: &Value) -> Result<Value, ToolError> {
        assert_eq!(args, &json!({ "sku": "WF-318" }));
        std::fs::File::create_new(self.state.join(self.name))
            .expect("each tool must have exactly one effect");
        Ok(json!(self.answer))
    }
}

async fn daemon_process(config: &Path) -> Result<(), DaemonError> {
    let state = config.parent().unwrap().to_owned();
    let runtime = Runtime::build(
        Config::load(config)?,
        BTreeMap::from([(
            "orders".into(),
            RegisteredSkill {
                description: "Catalog and reviewed orders".into(),
                tools: vec![
                    Box::new(OrderTool {
                        state: state.clone(),
                        name: "lookup",
                        answer: "350 cents",
                    }),
                    Box::new(OrderTool {
                        state: state.clone(),
                        name: "review",
                        answer: "reviewed",
                    }),
                ],
            },
        )]),
        |model| {
            assert_eq!(model, "scripted-order-provider");
            Ok(Box::new(OrderProvider))
        },
    )?;
    let listener = TcpListener::bind(runtime.bind).await?;
    std::fs::write(state.join("listen"), listener.local_addr()?.to_string())?;
    server::serve(listener, runtime, CancellationToken::new()).await
}

struct RunningDaemon {
    child: Child,
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn launch(config: &Path) -> (RunningDaemon, SocketAddr) {
    let ready = config.parent().unwrap().join("listen");
    if ready.exists() {
        std::fs::remove_file(&ready).unwrap();
    }
    let mut daemon = RunningDaemon {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::http_workflow_survives_daemon_restart",
                "--nocapture",
            ])
            .env(CHILD_CONFIG, config)
            .spawn()
            .unwrap(),
    };
    let address = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            if let Ok(value) = std::fs::read_to_string(&ready)
                && let Ok(address) = value.parse::<SocketAddr>()
            {
                break address;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (daemon, address)
}

async fn next_event(socket: &mut EventSocket) -> Value {
    let message = socket.next().await.unwrap().unwrap();
    let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
        panic!("expected journal event, received {message:?}");
    };
    serde_json::from_str(&text).unwrap()
}

async fn approvals(client: &Client, address: SocketAddr) -> Vec<Value> {
    client
        .get(format!("http://{address}/approvals"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn http_workflow_survives_daemon_restart() {
    if let Some(config) = std::env::var_os(CHILD_CONFIG) {
        daemon_process(Path::new(&config)).await.unwrap();
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("wakefold.json");
        std::fs::write(&config, json!({
            "state_directory": "state",
            "bind": "127.0.0.1:0",
            "max_concurrent": 1,
            "skills": {
                "delegate": {
                    "description": "Delegate work",
                    "tools": [
                        { "kind": "delegate", "name": "catalog", "description": "Read price", "agent": "catalog" },
                        { "kind": "delegate", "name": "review", "description": "Review order", "agent": "reviewer" }
                    ]
                }
            },
            "agents": {
                "coordinator": {
                    "model": "scripted-order-provider", "max_model_turns": 5, "token_budget": 19,
                    "skills": ["delegate"], "tool_grants": { "delegate.catalog": "allow", "delegate.review": "allow" }
                },
                "catalog": {
                    "model": "scripted-order-provider", "max_model_turns": 4, "token_budget": 10,
                    "skills": ["orders"], "tool_grants": { "orders.lookup": "allow" }
                },
                "reviewer": {
                    "model": "scripted-order-provider", "max_model_turns": 4, "token_budget": 13,
                    "skills": ["orders"], "tool_grants": { "orders.review": "require_approval" }
                },
                "no_budget": { "model": "scripted-order-provider", "max_model_turns": 1, "token_budget": 0 }
            }
        }).to_string()).unwrap();

        let client = Client::builder().no_proxy().build().unwrap();
        let request = json!({ "chat_key": "notebook-order", "run_key": "check", "agent": "coordinator", "prompt": PROMPT });
        let (daemon, address) = launch(&config).await;
        let response = client.post(format!("http://{address}/runs")).json(&request).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let started: Value = response.json().await.unwrap();
        let id = started["agent_id"].as_i64().unwrap();
        let (mut socket, _) = connect_async(format!("ws://{address}/runs/{id}/events")).await.unwrap();
        let mut after = 0;
        loop {
            let event = next_event(&mut socket).await;
            assert_eq!(event["sequence"], after + 1);
            after += 1;
            if event["event"]["kind"] == "wait_started"
                && event["event"]["data"]["on"]["Delegation"]["worker"] == "reviewer"
            {
                break;
            }
        }
        let pending = loop {
            let pending = approvals(&client, address).await;
            if !pending.is_empty() {
                break pending;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(pending.len(), 1);
        let wait = pending[0]["wait"]["id"].as_str().unwrap();
        assert_eq!(pending[0]["wait"]["on"]["ToolApproval"]["tool_name"], "orders.review");
        assert!(directory.path().join("lookup").exists());
        assert!(!directory.path().join("review").exists());

        // Kill the serving process with a WebSocket open and the review still pending.
        drop(daemon);
        drop(socket);
        let (_daemon, address) = launch(&config).await;
        assert_eq!(approvals(&client, address).await, pending);
        let retry = client.post(format!("http://{address}/runs")).json(&request).send().await.unwrap();
        assert_eq!(retry.status(), StatusCode::ACCEPTED);
        assert_eq!(retry.json::<Value>().await.unwrap(), started);

        let (mut socket, _) = connect_async(format!("ws://{address}/runs/{id}/events?after={after}")).await.unwrap();
        let decision = client.post(format!("http://{address}/approvals/{wait}"))
            .json(&json!({"approved": true})).send().await.unwrap();
        assert_eq!(decision.status(), StatusCode::NO_CONTENT);
        loop {
            let event = next_event(&mut socket).await;
            assert_eq!(event["sequence"], after + 1);
            after += 1;
            if event["event"]["kind"] == "run_finished" {
                assert_eq!(event["event"]["data"], FINAL_ANSWER);
                break;
            }
        }
        assert!(directory.path().join("review").exists());
        assert!(approvals(&client, address).await.is_empty());
        let repeated = client.post(format!("http://{address}/approvals/{wait}"))
            .json(&json!({"approved": true})).send().await.unwrap();
        assert_eq!(repeated.status(), StatusCode::NOT_FOUND);

        let no_budget: Value = client.post(format!("http://{address}/runs"))
            .json(&json!({"chat_key": "notebook-order", "run_key": "blocked", "agent": "no_budget", "prompt": "Must not start"}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let id = no_budget["agent_id"].as_i64().unwrap();
        let (mut socket, _) = connect_async(format!("ws://{address}/runs/{id}/events")).await.unwrap();
        assert_eq!(next_event(&mut socket).await["event"]["kind"], "run_started");
        let failure = next_event(&mut socket).await;
        assert_eq!(failure["event"]["kind"], "run_failed");
        assert!(failure["event"]["data"].as_str().unwrap().contains("token budget"));
    }).await.expect("the API workflow must finish");
}
