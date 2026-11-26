use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Response, StatusCode, header};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};
use wakefold::{
    CancellationToken, Message, ModelRequest, ModelResponse, ModelTurn, Provider, ProviderError,
    TokenUsage, Tool, ToolCall, ToolCallId, ToolContext, ToolError, ToolSpec,
};

use super::{CHILD_CONFIG, dispatch, launch, next_approval, next_event};
use crate::config::{Config, RegisteredSkill, Runtime};
use crate::{DaemonError, server};

const TEST: &str = "tests::artifacts::artifact_publication_survives_daemon_crash";
const STOP_AFTER_COPY: &str = "pause-after-artifact-copy";
const COPY_READY: &str = "artifact-copy-ready";
const DRAFT_NAME: &str = "inventory report.csv";
const ORIGINAL: &str = "item,quantity\nnotebook,4\n";
const UPDATED: &str = "item,quantity\nnotebook,6\n";
const FIRST_TASK: &str = "Write the first inventory report.";
const SECOND_TASK: &str = "Write the updated inventory report.";

struct PauseAfterCopy(PathBuf);

impl<S: Subscriber> Layer<S> for PauseAfterCopy {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "wakefold::artifact" {
            fs::write(&self.0, "ready").unwrap();
            loop {
                std::thread::park();
            }
        }
    }
}

struct ArtifactProvider;

fn tool_result<'a>(request: &ModelRequest<'a>, call_id: &str) -> &'a Value {
    let call_id = ToolCallId::from(call_id);
    request
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Tool(result) if result.call_id == call_id => Some(&result.value),
            _ => None,
        })
        .expect("previous tool result must survive")
}

#[async_trait]
impl Provider for ArtifactProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'chunk> FnMut(&'chunk str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        let response = self.complete_response(request).await?;
        Ok(ModelTurn::new(response, Some(TokenUsage::new(2, 1))))
    }

    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let Message::User(prompt) = &request.messages[0] else {
            panic!("expected a user prompt");
        };
        let draft = matches!(prompt.as_str(), FIRST_TASK | SECOND_TASK);
        let load = |name: &str| ToolCall::new("load", "load_skill", json!({ "name": name }));
        let response = match request.messages.last().unwrap() {
            Message::User(_) => load(if draft { "drafts" } else { "delegate" }).into(),
            Message::Tool(result) => {
                let work = |id, task| dispatch(id, "delegate.report", json!({ "task": task }));
                match result.call_id.to_string().as_str() {
                    "load" if draft => dispatch(
                        "write",
                        "drafts.write",
                        json!({ "updated": prompt == SECOND_TASK }),
                    )
                    .into(),
                    "write" => ModelResponse::Final(result.value.as_str().unwrap().into()),
                    "load" => work("first_draft", FIRST_TASK).into(),
                    "first_draft" => ToolCall::new(
                        "load_artifacts",
                        "load_skill",
                        json!({ "name": "artifacts" }),
                    )
                    .into(),
                    "load_artifacts" => {
                        assert_eq!(result.value["tools"][0]["name"], "artifacts.publish");
                        dispatch(
                            "first_publish",
                            "artifacts.publish",
                            json!({
                                "title": "Weekly inventory",
                                "path": tool_result(&request, "first_draft")["answer"],
                            }),
                        )
                        .into()
                    }
                    "first_publish" => {
                        assert_eq!(result.value["revision"], 1);
                        work("second_draft", SECOND_TASK).into()
                    }
                    "second_draft" => dispatch(
                        "second_publish",
                        "artifacts.publish",
                        json!({
                            "title": "Weekly inventory",
                            "path": result.value["answer"],
                            "artifact_id": tool_result(&request, "first_publish")["artifact_id"],
                        }),
                    )
                    .into(),
                    "second_publish" => {
                        assert_eq!(result.value["revision"], 2);
                        ModelResponse::Final("Published both revisions.".into())
                    }
                    id => panic!("unexpected result {id}"),
                }
            }
            other => panic!("unexpected message {other:?}"),
        };
        Ok(response)
    }
}

struct DraftReport;

#[async_trait]
impl Tool for DraftReport {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("write", "Write an inventory CSV", json!({"type": "object"}))
    }

    async fn call(&self, _: &Value) -> Result<Value, ToolError> {
        unreachable!("test report needs a workspace")
    }

    async fn call_with_context(
        &self,
        context: ToolContext,
        arguments: &Value,
    ) -> Result<Value, ToolError> {
        let workspace = context.workspace().unwrap();
        let path = format!("agents/{}/{DRAFT_NAME}", workspace.agent_id());
        let bytes = if arguments["updated"].as_bool().unwrap() {
            UPDATED
        } else {
            ORIGINAL
        };
        fs::write(workspace.chat_root().join(&path), bytes)
            .map_err(|error| ToolError::with_source("could not write inventory report", error))?;
        Ok(json!(path))
    }
}

async fn daemon(config: &Path) -> Result<(), DaemonError> {
    let directory = config.parent().unwrap();
    let pause = directory
        .join(STOP_AFTER_COPY)
        .exists()
        .then(|| PauseAfterCopy(directory.join(COPY_READY)));
    Registry::default()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr),
        )
        .with(pause)
        .try_init()?;

    let runtime = Runtime::build(
        Config::load(config)?,
        BTreeMap::from([(
            "drafts".into(),
            RegisteredSkill {
                description: "Write inventory reports".into(),
                tools: vec![Box::new(DraftReport)],
            },
        )]),
        |_| Ok(Box::new(ArtifactProvider)),
    )
    .await?;
    let socket = TcpListener::bind(runtime.bind).await?;
    fs::write(directory.join("listen"), socket.local_addr()?.to_string())?;
    server::serve(socket, runtime, CancellationToken::new()).await
}

async fn catalog(client: &Client, address: SocketAddr, chat: &str) -> Vec<Value> {
    client
        .get(format!("http://{address}/artifacts"))
        .query(&[("chat_key", chat)])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn download(
    client: &Client,
    address: SocketAddr,
    chat: &str,
    artifact_id: i64,
    revision: u64,
) -> Response {
    client
        .get(format!(
            "http://{address}/artifacts/{artifact_id}/revisions/{revision}"
        ))
        .query(&[("chat_key", chat)])
        .send()
        .await
        .unwrap()
}

async fn approve(client: &Client, address: SocketAddr, wait: &str) {
    let response = client
        .post(format!("http://{address}/approvals/{wait}"))
        .json(&json!({"approved": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn artifact_publication_survives_daemon_crash() {
    if let Some(config) = std::env::var_os(CHILD_CONFIG) {
        daemon(Path::new(&config)).await.unwrap();
        return;
    }
    tokio::time::timeout(Duration::from_secs(35), async {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let config = root.join("wakefold.json");
        fs::write(root.join(STOP_AFTER_COPY), "").unwrap();
        fs::write(&config, json!({
            "state_directory": "state",
            "bind": "127.0.0.1:0",
            "skills": {
                "delegate": {
                    "description": "Ask the analyst to write a report",
                    "tools": [{"kind": "delegate", "name": "report", "description": "Write a report", "agent": "analyst"}]
                }
            },
            "agents": {
                "coordinator": {
                    "model": "scripted", "max_model_turns": 10, "token_budget": 100,
                    "skills": ["delegate", "artifacts"],
                    "tool_grants": {"delegate.report": "allow", "artifacts.publish": "require_approval"}
                },
                "analyst": {
                    "model": "scripted", "max_model_turns": 4, "token_budget": 100,
                    "skills": ["drafts"], "tool_grants": {"drafts.write": "allow"}
                }
            }
        }).to_string()).unwrap();

        let client = Client::builder().no_proxy().build().unwrap();
        let (child, address) = launch(&config, TEST).await;
        let start: Value = client.post(format!("http://{address}/runs"))
            .json(&json!({
                "chat_key": "inventory", "run_key": "report", "agent": "coordinator",
                "prompt": "Publish two revisions of the inventory report."
            }))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let run = start["agent_id"].as_i64().unwrap();

        let approval = next_approval(&client, address, "").await;
        assert_eq!(approval["agent_id"], run);
        let wait = &approval["wait"];
        let first_wait = wait["id"].as_str().unwrap();
        let publish = &wait["on"]["ToolApproval"];
        assert_eq!(publish["tool_name"], "artifacts.publish");
        let path = publish["arguments"]["path"].as_str().unwrap();
        assert!(path.ends_with(DRAFT_NAME));
        assert!(catalog(&client, address, "inventory").await.is_empty());
        approve(&client, address, first_wait).await;

        while !root.join(COPY_READY).exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(catalog(&client, address, "inventory").await.is_empty(), "a saved file is visible only after publication commits");
        let workspace = fs::read_dir(root.join("state/workspaces")).unwrap().next().unwrap().unwrap().path();
        assert_eq!(fs::read_to_string(workspace.join(path)).unwrap(), ORIGINAL);
        drop(child);
        fs::write(workspace.join(path), "edited after the copy").unwrap();
        fs::remove_file(root.join(STOP_AFTER_COPY)).unwrap();

        let (_child, address) = launch(&config, TEST).await;
        let second = next_approval(&client, address, first_wait).await;
        assert_eq!(second["agent_id"], run);
        let published = catalog(&client, address, "inventory").await;
        assert_eq!(published.len(), 1);
        let artifact_id = published[0]["artifact_id"].as_i64().unwrap();
        assert_eq!(published[0]["title"], "Weekly inventory");
        assert_eq!(published[0]["revision"], 1);
        assert_eq!(published[0]["filename"], DRAFT_NAME);
        let original = download(&client, address, "inventory", artifact_id, 1).await;
        assert_eq!(original.status(), StatusCode::OK);
        assert_eq!(original.headers()[header::CONTENT_TYPE], "text/csv");
        assert_eq!(original.headers()[header::CONTENT_DISPOSITION], "attachment; filename*=UTF-8''inventory%20report%2Ecsv");
        assert_eq!(original.text().await.unwrap(), ORIGINAL);
        assert_eq!(download(&client, address, "other-chat", artifact_id, 1).await.status(), StatusCode::NOT_FOUND);
        assert!(catalog(&client, address, "other-chat").await.is_empty());

        let wait = &second["wait"];
        assert_eq!(wait["on"]["ToolApproval"]["arguments"]["artifact_id"], artifact_id);
        let (mut socket, _) = connect_async(format!("ws://{address}/runs/{run}/events")).await.unwrap();
        approve(&client, address, wait["id"].as_str().unwrap()).await;
        let mut revisions = Vec::new();
        let mut results = Vec::new();
        loop {
            let frame = next_event(&mut socket).await;
            let event = &frame["event"];
            match event["kind"].as_str() {
                Some("artifact_published") => revisions.push(event["data"].clone()),
                Some("tool_call_completed") if event["data"]["value"]["artifact_id"] == artifact_id => {
                    results.push(event["data"]["value"].clone());
                }
                Some("run_finished") => {
                    assert_eq!(event["data"], "Published both revisions.");
                    break;
                }
                Some("run_failed") => panic!("run failed {event}"),
                _ => {}
            }
        }
        assert_eq!(revisions, results);
        assert_eq!(revisions.len(), 2, "recovery must publish exactly one revision");
        assert_eq!(revisions[0], published[0]);
        assert_eq!(revisions[1]["artifact_id"], artifact_id);
        assert_eq!(revisions[1]["revision"], 2);
        assert_eq!(catalog(&client, address, "inventory").await, revisions[1..]);
        for (revision, expected) in [(1, ORIGINAL), (2, UPDATED)] {
            let response = download(&client, address, "inventory", artifact_id, revision).await;
            assert_eq!(response.error_for_status().unwrap().text().await.unwrap(), expected);
        }
    }).await.expect("the artifact workflow must finish");
}
