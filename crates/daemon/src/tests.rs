mod artifacts;
mod continuation;
mod dynamic;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
use crate::{DaemonError, replay, server};

const CHILD_CONFIG: &str = "WAKEFOLD_DAEMON_TEST_CONFIG";
const ORDER_TEST: &str = "tests::http_workflow_survives_daemon_restart";
const MAILBOX_TEST: &str = "tests::http_messages_survive_daemon_restart";
const MCP_TEST: &str = "tests::mcp_tool_works_after_daemon_restart";
const PROMPT: &str = "Find the notebook price and record an approved review.";
const FINAL_ANSWER: &str = "The notebook costs 350 cents and its review was recorded.";
const MAILBOX_PROMPT: &str = "After approval, receive two client messages.";
const MCP_PROMPT: &str = "Look up prices for WF-318 and WF-319 in the MCP catalog.";
type EventSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct WorkflowProvider;

fn dispatch(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall::new(
        id,
        "call_tool",
        json!({ "tool_name": name, "arguments": arguments }),
    )
}

#[async_trait]
impl Provider for WorkflowProvider {
    async fn complete_response(&self, _: ModelRequest<'_>) -> Result<ModelResponse, ProviderError> {
        unreachable!("the driver requests token usage with each turn")
    }

    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let Some(Message::User(prompt)) = request.messages.get(1) else {
            panic!("the original prompt must survive");
        };
        if prompt == MAILBOX_PROMPT {
            return Ok(ModelTurn::new(
                mailbox_response(request.messages.last().unwrap()),
                Some(TokenUsage::new(2, 1)),
            ));
        }
        if prompt == MCP_PROMPT {
            assert_eq!(
                request
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                ["load_skill", "call_tool"]
            );
            return Ok(ModelTurn::new(
                mcp_response(request.messages.last().unwrap()),
                Some(TokenUsage::new(2, 1)),
            ));
        }
        let skill = if prompt == PROMPT { "agents" } else { "orders" };
        let response = match request.messages.last().unwrap() {
            Message::Developer(_) => {
                ToolCall::new("load", "load_skill", json!({ "name": skill })).into()
            }
            Message::Tool(result) if result.call_id == "load".into() => match prompt.as_str() {
                PROMPT => ModelResponse::ToolCalls {
                    text: None,
                    calls: vec![
                        dispatch(
                            "catalog",
                            "agents.spawn",
                            json!({ "task": "Read the price." }),
                        ),
                        dispatch(
                            "review",
                            "agents.spawn",
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
                let child = |call: &str| {
                    request
                        .messages
                        .iter()
                        .find_map(|m| match m {
                            Message::Tool(r) if r.call_id.as_str() == call => {
                                r.value["agent_id"].as_i64()
                            }
                            _ => None,
                        })
                        .unwrap()
                };
                match result.call_id.as_str() {
                    "review" => {
                        ToolCall::new("mail", "load_skill", json!({"name":"mailbox"})).into()
                    }
                    "mail" => dispatch(
                        "price",
                        "mailbox.receive",
                        json!({"completion_only":true,"from_agent_id":child("catalog")}),
                    )
                    .into(),
                    "price" => {
                        assert_eq!(result.value["payload"]["answer"], "350 cents");
                        dispatch(
                            "receipt",
                            "mailbox.receive",
                            json!({"completion_only":true,"from_agent_id":child("review")}),
                        )
                        .into()
                    }
                    "receipt" => {
                        assert_eq!(result.value["payload"]["answer"], "reviewed");
                        ModelResponse::Final(FINAL_ANSWER.into())
                    }
                    other => panic!("unexpected coordinator result {other}"),
                }
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

fn mailbox_response(message: &Message) -> ModelResponse {
    match message {
        Message::Developer(_) => {
            ToolCall::new("load", "load_skill", json!({ "name": "notices" })).into()
        }
        Message::Tool(result) if result.call_id == "load".into() => {
            dispatch("timer", "notices.wait", json!({ "seconds": 0 })).into()
        }
        Message::Tool(result) if result.call_id == "timer".into() => {
            dispatch("first", "notices.receive", json!({})).into()
        }
        Message::Tool(result) if result.call_id == "first".into() => {
            assert_eq!(
                result.value["payload"],
                json!({"status": "queued before waiting"})
            );
            assert_eq!(result.value.get("from_agent_id"), Some(&Value::Null));
            dispatch("second", "notices.receive", json!({})).into()
        }
        Message::Tool(result) => {
            assert_eq!(result.call_id, "second".into());
            assert_eq!(
                result.value["payload"],
                json!({"status": "arrived while waiting"})
            );
            assert_eq!(result.value.get("from_agent_id"), Some(&Value::Null));
            ModelResponse::Final("Received both messages".into())
        }
        other => panic!("unexpected mailbox message {other:?}"),
    }
}

fn mcp_response(message: &Message) -> ModelResponse {
    match message {
        Message::Developer(_) => {
            ToolCall::new("load", "load_skill", json!({ "name": "catalog" })).into()
        }
        Message::Tool(result) if result.call_id == "load".into() => {
            let tools = result.value["tools"].as_array().unwrap();
            assert_eq!(
                tools.len(),
                1,
                "only configured MCP tools should be exposed"
            );
            assert_eq!(tools[0]["name"], "catalog.lookup_price");
            assert_eq!(
                tools[0]["description"],
                "Look up the catalog price in cents."
            );
            assert_eq!(tools[0]["parameters"]["required"], json!(["sku"]));
            assert_eq!(
                tools[0]["parameters"]["properties"]["sku"]["type"],
                "string"
            );
            dispatch("notebook", "catalog.lookup_price", json!({"sku": "WF-318"})).into()
        }
        Message::Tool(result) if result.call_id == "notebook".into() => {
            assert_eq!(
                result.value["structuredContent"],
                json!({"sku": "WF-318", "price_cents": 350})
            );
            assert_eq!(
                result.value["content"][0]["text"],
                "WF-318 costs 350 cents."
            );
            assert!(result.value.get("_meta").is_none());
            dispatch("binder", "catalog.lookup_price", json!({"sku": "WF-319"})).into()
        }
        Message::Tool(result) => {
            assert_eq!(result.call_id, "binder".into());
            assert_eq!(
                result.value["structuredContent"],
                json!({"sku": "WF-319", "price_cents": 425})
            );
            ModelResponse::Final("Catalog prices are 350 and 425 cents.".into())
        }
        other => panic!("unexpected catalog message {other:?}"),
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
    crate::init_logging()?;
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
            assert_eq!(model, "scripted-workflow-provider");
            Ok(Box::new(WorkflowProvider))
        },
    )
    .await?;
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

async fn launch(config: &Path, test: &str) -> (RunningDaemon, SocketAddr) {
    let ready = config.parent().unwrap().join("listen");
    let log_path = config.parent().unwrap().join("daemon.jsonl");
    let log_file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    if ready.exists() {
        std::fs::remove_file(&ready).unwrap();
    }
    let mut daemon = RunningDaemon {
        child: Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_CONFIG, config)
            .stderr(Stdio::from(log_file))
            .spawn()
            .unwrap(),
    };
    let address = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited during startup\n{}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
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

fn saved_replay(state: &Path, agent_id: i64) -> Vec<Value> {
    let mut output = Vec::new();
    replay::write(state, agent_id, &mut output).unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
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
            "defaults": {"model":"scripted-workflow-provider","max_model_turns":12,"token_budget":40,
                "tool_grants":{"orders.review":"require_approval"}}
        }).to_string()).unwrap();

        let client = Client::builder().no_proxy().build().unwrap();
        let request = json!({ "chat_id": wakefold::ChatId::from_u128(1), "run_key": "check", "prompt": PROMPT });
        let (daemon, address) = launch(&config, ORDER_TEST).await;
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
                && event["event"]["data"]["on"].get("ChildOutcome").is_some()
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
        let state = directory.path().join("state");
        let review_id = pending[0]["agent_id"].as_i64().unwrap();
        let waiting = saved_replay(&state, id);
        assert!(waiting.iter().any(|frame| frame["agent_id"] == review_id
            && frame["event"]["kind"] == "wait_started" && frame["event"]["data"]["id"] == wait));
        assert!(!waiting.iter().any(|frame| frame["event"]["kind"] == "woken"
            && frame["event"]["data"]["wait_id"] == wait));

        // Kill the serving process with a WebSocket open and the review still pending.
        drop(daemon);
        drop(socket);
        let (daemon, address) = launch(&config, ORDER_TEST).await;
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
        let events = saved_replay(&state, id);
        let approval_requested = events.iter().position(|frame| frame["agent_id"] == review_id
            && frame["event"]["kind"] == "wait_started" && frame["event"]["data"]["id"] == wait).unwrap();
        let approval_granted = events.iter().position(|frame| frame["agent_id"] == review_id
            && frame["event"]["kind"] == "woken" && frame["event"]["data"]["wait_id"] == wait
            && frame["event"]["data"]["payload"]["approved"] == true).unwrap();
        let worker_finished = events.iter().position(|frame| frame["agent_id"] == review_id
            && frame["event"]["kind"] == "run_finished" && frame["event"]["data"] == "reviewed").unwrap();
        let worker_result = events.iter().position(|frame| frame["agent_id"] == id
            && frame["event"]["kind"] == "woken" && frame["event"]["data"]["payload"]["payload"]["answer"] == "reviewed").unwrap();
        let coordinator_finished = events.iter().position(|frame| frame["agent_id"] == id
            && frame["event"]["kind"] == "run_finished" && frame["event"]["data"] == FINAL_ANSWER).unwrap();
        assert!(approval_requested < approval_granted && approval_granted < worker_finished
            && worker_result < coordinator_finished);
        let log = std::fs::read_to_string(directory.path().join("daemon.jsonl")).unwrap();
        let logs: Vec<Value> = log.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        for frame in &events {
            assert!(logs.iter().any(|log| {
                let fields = &log["fields"];
                fields["agent_id"] == frame["agent_id"] && fields["sequence"] == frame["sequence"]
                    && fields["event"] == frame["event"]["kind"]
            }), "missing committed trace for agent {} sequence {}", frame["agent_id"], frame["sequence"]);
        }
        assert!(logs.iter().any(|log| log["fields"]["agent_id"] == review_id
            && log["fields"]["parent_agent_id"] == id));
        assert!(logs.iter().any(|log| log["fields"]["agent_id"] == review_id
            && log["fields"]["wait_id"] == wait && log["fields"]["approved"] == true));
        assert!(!log.contains(PROMPT));
        let repeated = client.post(format!("http://{address}/approvals/{wait}"))
            .json(&json!({"approved": true})).send().await.unwrap();
        assert_eq!(repeated.status(), StatusCode::NOT_FOUND);

        drop(daemon);
        let mut settings: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        settings["defaults"]["token_budget"] = json!(0);
        std::fs::write(&config, settings.to_string()).unwrap();
        let (_daemon, address) = launch(&config, ORDER_TEST).await;
        let no_budget: Value = client.post(format!("http://{address}/runs"))
            .json(&json!({"chat_id": wakefold::ChatId::from_u128(4), "run_key": "blocked", "prompt": "Must not start"}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let id = no_budget["agent_id"].as_i64().unwrap();
        let (mut socket, _) = connect_async(format!("ws://{address}/runs/{id}/events")).await.unwrap();
        assert_eq!(next_event(&mut socket).await["event"]["kind"], "run_started");
        assert_eq!(next_event(&mut socket).await["event"]["kind"], "startup_context");
        let failure = next_event(&mut socket).await;
        assert_eq!(failure["event"]["kind"], "run_failed");
        assert!(failure["event"]["data"].as_str().unwrap().contains("token budget"));
    }).await.expect("the API workflow must finish");
}

#[tokio::test]
async fn http_messages_survive_daemon_restart() {
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
            "skills": {
                "notices": {"description":"Wait for a notice", "tools":[{"kind":"timer_wait"},{"kind":"wait_for_message"}]}
            },
            "defaults": {"model":"scripted-workflow-provider","max_model_turns":6,"token_budget":30,
                "tool_grants":{"notices.wait":"require_approval"}}
        }).to_string()).unwrap();

        let client = Client::builder().no_proxy().build().unwrap();
        let (daemon, address) = launch(&config, MAILBOX_TEST).await;
        let response = client.post(format!("http://{address}/runs")).json(&json!({
            "chat_id": wakefold::ChatId::from_u128(2), "run_key": "mailbox-check", "prompt": MAILBOX_PROMPT
        })).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
        let agent_id = response["agent_id"].as_i64().unwrap();
        let pending = loop {
            let pending = approvals(&client, address).await;
            if !pending.is_empty() {
                break pending;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["agent_id"], agent_id);
        assert_eq!(pending[0]["wait"]["on"]["ToolApproval"]["tool_name"], "notices.wait");
        let wait = pending[0]["wait"]["id"].as_str().unwrap();

        // The approval guarantees there is no mailbox wait yet.
        let first = json!({ "message_key": "notice-1", "payload": { "status": "queued before waiting" } });
        let posted = client.post(format!("http://{address}/runs/{agent_id}/messages"))
            .json(&first).send().await.unwrap();
        assert_eq!(posted.status(), StatusCode::ACCEPTED);
        let first_id = posted.json::<Value>().await.unwrap()["message_id"].as_str().unwrap().to_owned();
        drop(daemon);

        let (_daemon, address) = launch(&config, MAILBOX_TEST).await;
        let messages = format!("http://{address}/runs/{agent_id}/messages");
        let duplicate = client.post(&messages).json(&first).send().await.unwrap();
        assert_eq!(duplicate.status(), StatusCode::ACCEPTED);
        assert_eq!(duplicate.json::<Value>().await.unwrap()["message_id"], first_id);
        let conflict = client.post(&messages).json(&json!({ "message_key": "notice-1", "payload": "changed" }))
            .send().await.unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let (mut socket, _) = connect_async(format!("ws://{address}/runs/{agent_id}/events")).await.unwrap();
        let decision = client.post(format!("http://{address}/approvals/{wait}"))
            .json(&json!({"approved": true})).send().await.unwrap();
        assert_eq!(decision.status(), StatusCode::NO_CONTENT);
        let mut mailbox_waits = 0;
        let mut received = Vec::new();
        while mailbox_waits < 2 {
            let frame = next_event(&mut socket).await;
            let event = &frame["event"];
            if event["kind"] == "wait_started" && event["data"]["on"].get("MailboxMessage").is_some() {
                mailbox_waits += 1;
            }
            if event["kind"] == "woken"
                && let Some(id) = event["data"]["payload"]["message_id"].as_str()
            {
                received.push(id.to_owned());
            }
            assert_ne!(event["kind"], "run_failed", "{event}");
        }
        assert_eq!(received, [first_id.as_str()]);

        let second = client.post(&messages)
            .json(&json!({ "message_key": "notice-2", "payload": { "status": "arrived while waiting" } }))
            .send().await.unwrap();
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        let second_id = second.json::<Value>().await.unwrap()["message_id"].as_str().unwrap().to_owned();
        loop {
            let frame = next_event(&mut socket).await;
            let event = &frame["event"];
            if event["kind"] == "woken"
                && let Some(id) = event["data"]["payload"]["message_id"].as_str()
            {
                received.push(id.to_owned());
            }
            if event["kind"] == "run_finished" {
                assert_eq!(event["data"], "Received both messages");
                break;
            }
            assert_ne!(event["kind"], "run_failed", "{event}");
        }
        assert_eq!(received, [first_id, second_id]);
    }).await.expect("the HTTP mailbox workflow must finish");
}

async fn next_approval(client: &Client, address: SocketAddr, previous: &str) -> Value {
    loop {
        if let Some(approval) = approvals(client, address)
            .await
            .into_iter()
            .find(|approval| {
                approval["wait"]["id"]
                    .as_str()
                    .is_some_and(|id| id != previous)
            })
        {
            return approval;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn mcp_tool_works_after_daemon_restart() {
    if let Some(config) = std::env::var_os(CHILD_CONFIG) {
        daemon_process(Path::new(&config)).await.unwrap();
        return;
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("wakefold.json");
        std::fs::write(directory.path().join("catalog_mcp.py"),
            include_str!("../tests/fixtures/catalog_mcp.py")).unwrap();
        std::fs::write(&config, json!({
            "state_directory": "state",
            "bind": "127.0.0.1:0",
            "mcp_servers": {
                "inventory": { "command": "python3", "args": ["catalog_mcp.py"] }
            },
            "skills": {
                "catalog": {
                    "description": "Read the catalog",
                    "tools": [
                        { "kind": "mcp", "server": "inventory", "tool": "lookup.price", "name": "lookup_price" }
                    ]
                }
            },
            "defaults": {"model":"scripted-workflow-provider","max_model_turns":5,"token_budget":30,
                "tool_grants":{"catalog.lookup_price":"require_approval"}}
        }).to_string()).unwrap();
        let calls_path = directory.path().join("catalog-calls.jsonl");
        let client = Client::builder().no_proxy().build().unwrap();
        let (daemon, address) = launch(&config, MCP_TEST).await;
        let response = client.post(format!("http://{address}/runs")).json(&json!({
            "chat_id": wakefold::ChatId::from_u128(3), "run_key": "prices", "prompt": MCP_PROMPT
        })).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
        let agent_id = response["agent_id"].as_i64().unwrap();
        let first = next_approval(&client, address, "").await;
        assert_eq!(first["agent_id"], agent_id);
        assert_eq!(first["wait"]["on"]["ToolApproval"]["tool_name"], "catalog.lookup_price");
        assert_eq!(first["wait"]["on"]["ToolApproval"]["arguments"], json!({"sku": "WF-318"}));
        assert!(!calls_path.exists());
        let first_wait = first["wait"]["id"].as_str().unwrap();
        let approval = client.post(format!("http://{address}/approvals/{first_wait}"))
            .json(&json!({"approved": true})).send().await.unwrap();
        assert_eq!(approval.status(), StatusCode::NO_CONTENT);

        let second = next_approval(&client, address, first_wait).await;
        assert_eq!(second["agent_id"], agent_id);
        assert_eq!(second["wait"]["on"]["ToolApproval"]["arguments"], json!({"sku": "WF-319"}));
        let calls = std::fs::read_to_string(&calls_path).unwrap();
        let first_call: Value = serde_json::from_str(&calls).unwrap();
        assert_eq!(first_call["sku"], "WF-318");
        drop(daemon);

        let (_daemon, address) = launch(&config, MCP_TEST).await;
        assert_eq!(approvals(&client, address).await.as_slice(), std::slice::from_ref(&second));
        let second_wait = second["wait"]["id"].as_str().unwrap();
        let (mut socket, _) = connect_async(format!("ws://{address}/runs/{agent_id}/events")).await.unwrap();
        let approval = client.post(format!("http://{address}/approvals/{second_wait}"))
            .json(&json!({"approved": true})).send().await.unwrap();
        assert_eq!(approval.status(), StatusCode::NO_CONTENT);
        let mut load_count = 0;
        let mut call_count = 0;
        loop {
            let frame = next_event(&mut socket).await;
            let event = &frame["event"];
            match event["kind"].as_str() {
                Some("skill_loaded") => {
                    assert_eq!(event["data"], "catalog");
                    load_count += 1;
                }
                Some("tool_call_started") if event["data"]["call"]["name"] == "call_tool" => {
                    assert_eq!(event["data"]["recovery"], "stop_after_crash");
                    call_count += 1;
                }
                Some("run_finished") => {
                    assert_eq!(event["data"], "Catalog prices are 350 and 425 cents.");
                    break;
                }
                Some("run_failed") => panic!("MCP workflow failed {event}"),
                _ => {}
            }
        }
        assert_eq!(load_count, 1);
        assert_eq!(call_count, 2);
        let calls: Vec<Value> = std::fs::read_to_string(calls_path).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], first_call);
        assert_eq!(calls[1]["sku"], "WF-319");
        assert_ne!(calls[0]["pid"], calls[1]["pid"], "the daemon must launch a new MCP process");
    }).await.expect("the MCP workflow must finish");
}
