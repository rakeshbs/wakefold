//! Exercise dynamic agents across a real daemon process restart.
use super::*;

const TEST: &str = "tests::dynamic::parent_and_children_resume_after_restart";

struct DynamicProvider(String);

#[async_trait]
impl Provider for DynamicProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        Ok(ModelTurn::new(
            self.complete_response(request).await?,
            Some(TokenUsage::new(3, 2)),
        ))
    }

    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        assert_eq!(self.0, "original");
        let [
            Message::Developer(instructions),
            Message::User(task),
            Message::Developer(identity),
            ..,
        ] = request.messages
        else {
            panic!("missing startup context")
        };
        assert!(instructions.starts_with("Coordinate research"));
        assert!(identity.starts_with("Your agent ID is "));
        assert!(
            request
                .request_context
                .unwrap()
                .ends_with("(Asia/Kolkata).")
        );
        let root = task == "coordinate";
        assert_eq!(instructions.contains("\nagents:"), root);
        assert_eq!(request.tools[0].description.contains("\nagents:"), root);
        let last = match request.messages.last().unwrap() {
            Message::Tool(r) => r.call_id.as_str(),
            Message::Developer(_) => "",
            other => panic!("unexpected turn {other:?}"),
        };
        let result = |id: &str| {
            request
                .messages
                .iter()
                .find_map(|m| match m {
                    Message::Tool(r) if r.call_id.as_str() == id => Some(&r.value),
                    _ => None,
                })
                .unwrap()
        };
        let load = |id: &str, name: &str| ToolCall::new(id, "load_skill", json!({"name":name}));
        let call = match (task.as_str(), last) {
            ("coordinate", "") => load("agents", "agents"),
            ("coordinate", "agents") => {
                dispatch("first", "agents.spawn", json!({"task":"research"}))
            }
            ("coordinate", "first") => dispatch(
                "second",
                "agents.spawn",
                json!({"task":"attempt nested spawn"}),
            ),
            ("coordinate", "second") => load("mail", "mailbox"),
            ("coordinate", "mail") => dispatch(
                "ask",
                "mailbox.send",
                json!({"to_agent_id":result("first")["agent_id"],"payload":"status?"}),
            ),
            ("coordinate", "ask") => load("orders", "orders"),
            ("coordinate", "orders") => {
                dispatch("lookup", "orders.lookup", json!({"sku":"WF-318"}))
            }
            ("coordinate", "lookup") => dispatch(
                "status",
                "mailbox.receive",
                json!({"from_agent_id":result("first")["agent_id"]}),
            ),
            ("coordinate", "status") => {
                assert_eq!(result("status")["payload"], "working");
                dispatch(
                    "check",
                    "mailbox.receive",
                    json!({"from_agent_id":result("first")["agent_id"],"wait":false,"completion_only":true}),
                )
            }
            ("coordinate", "check") => {
                assert!(result("check").is_null());
                dispatch(
                    "final",
                    "mailbox.receive",
                    json!({"from_agent_id":result("first")["agent_id"],"completion_only":true}),
                )
            }
            ("coordinate", "final") => {
                assert_eq!(result("final")["payload"]["answer"], "report ready");
                dispatch(
                    "failure",
                    "mailbox.receive",
                    json!({"from_agent_id":result("second")["agent_id"],"completion_only":true}),
                )
            }
            ("coordinate", "failure") => {
                assert_eq!(result("failure")["payload"]["kind"], "failed");
                return Ok(ModelResponse::Final("done".into()));
            }
            ("research", "") => load("mail", "mailbox"),
            ("research", "mail") => dispatch("request", "mailbox.receive", json!({})),
            ("research", "request") => {
                assert_eq!(result("request")["payload"], "status?");
                dispatch(
                    "progress",
                    "mailbox.send",
                    json!({"to_agent_id":result("request")["from_agent_id"],"payload":"working"}),
                )
            }
            ("research", "progress") => dispatch("release", "mailbox.receive", json!({})),
            ("research", "release") => return Ok(ModelResponse::Final("report ready".into())),
            ("attempt nested spawn", "") => load("mail", "mailbox"),
            ("attempt nested spawn", "mail") => dispatch("release", "mailbox.receive", json!({})),
            ("attempt nested spawn", "release") => {
                dispatch("forged", "agents.spawn", json!({"task":"grandchild"}))
            }
            other => panic!("unexpected task and call {other:?}"),
        };
        Ok(call.into())
    }
}

#[tokio::test]
async fn parent_and_children_resume_after_restart() {
    if let Some(path) = std::env::var_os(CHILD_CONFIG) {
        let path = PathBuf::from(path);
        let skills = BTreeMap::from([(
            "orders".into(),
            RegisteredSkill {
                description: "Read order prices".into(),
                tools: vec![Box::new(OrderTool {
                    state: path.parent().unwrap().into(),
                    name: "lookup",
                    answer: "350 cents",
                })],
            },
        )]);
        let runtime = Runtime::build(Config::load(&path).unwrap(), skills, |model| {
            Ok(Box::new(DynamicProvider(model.into())))
        })
        .await
        .unwrap();
        let listener = TcpListener::bind(runtime.bind).await.unwrap();
        std::fs::write(
            path.parent().unwrap().join("listen"),
            listener.local_addr().unwrap().to_string(),
        )
        .unwrap();
        server::serve(listener, runtime, CancellationToken::new())
            .await
            .unwrap();
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("config.json");
    let mut config = json!({"state_directory":"state","bind":"127.0.0.1:0","defaults":{
        "model":"original","instructions":"Coordinate research","max_model_turns":20,"token_budget":5000}});
    std::fs::write(&path, config.to_string()).unwrap();
    let client = Client::builder().no_proxy().build().unwrap();
    let chat = wakefold::ChatId::new_v4();
    let (process, address) = launch(&path, TEST).await;
    let request =
        json!({"chat_id":chat,"run_key":"root","prompt":"coordinate","time_zone":"Asia/Kolkata"});
    let root = client
        .post(format!("http://{address}/runs"))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["agent_id"]
        .as_i64()
        .unwrap();
    let tree = wait_for_tree(&client, address, chat, "wait_started").await;
    assert!(
        tree.iter()
            .all(|r| r["status"] == "waiting" && r.get("outcome") == Some(&Value::Null))
    );
    assert!(
        tree.iter()
            .skip(1)
            .all(|r| r["parent_agent_id"] == root && r["spawned_by_run_id"] == tree[0]["run_id"])
    );
    assert!(state.path().join("lookup").is_file());
    drop(process);
    config["defaults"]["model"] = json!("updated");
    std::fs::write(&path, config.to_string()).unwrap();
    let (_process, address) = launch(&path, TEST).await;
    assert_eq!(
        wait_for_tree(&client, address, chat, "wait_started").await,
        tree
    );
    let retry = client
        .post(format!("http://{address}/runs"))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(retry["agent_id"], root);
    for child in tree.iter().skip(1).map(|r| r["agent_id"].as_i64().unwrap()) {
        client
            .post(format!("http://{address}/runs/{child}/messages"))
            .json(&json!({"message_key":"release","payload":"finish"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let after = wait_for_tree(&client, address, chat, "run_finished").await;
    assert_eq!(
        tree.iter().map(|r| &r["agent_id"]).collect::<Vec<_>>(),
        after.iter().map(|r| &r["agent_id"]).collect::<Vec<_>>()
    );
    assert_eq!(after[0]["outcome"], "done");
    assert_eq!(after[1]["outcome"], "report ready");
    assert!(after[2]["outcome"].as_str().is_some_and(|s| !s.is_empty()));
    let messages = client
        .get(format!("http://{address}/runs/{root}/messages"))
        .send()
        .await
        .unwrap()
        .json::<Vec<Value>>()
        .await
        .unwrap();
    assert_eq!(messages.len(), 3);
    assert!(
        messages
            .iter()
            .all(|m| m["received_event_sequence"].is_number())
    );
    let history = saved_replay(&state.path().join("state"), root);
    for child in &after[1..] {
        let outcomes: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["from_agent_id"] == child["agent_id"]
                    && message["message_kind"] == "child_outcome"
            })
            .collect();
        let [outcome] = outcomes.as_slice() else {
            panic!("expected one outcome for {child}")
        };
        let receipts: Vec<_> = history
            .iter()
            .filter(|frame| {
                let event = &frame["event"];
                let delivered = match event["kind"].as_str() {
                    Some("woken") => &event["data"]["payload"],
                    Some("tool_call_completed") => &event["data"]["value"],
                    _ => return false,
                };
                frame["agent_id"] == root && delivered["message_id"] == outcome["message_id"]
            })
            .map(|frame| &frame["sequence"])
            .collect();
        assert_eq!(receipts, [&outcome["received_event_sequence"]]);
    }
}

async fn wait_for_tree(
    client: &Client,
    address: SocketAddr,
    chat: wakefold::ChatId,
    root_state: &str,
) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let tree = client
                .get(format!("http://{address}/chats/{chat}/runs"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Vec<Value>>()
                .await
                .unwrap();
            if tree.len() == 3
                && tree[0]["latest_event"] == root_state
                && tree[1]["latest_event"]
                    == if root_state == "wait_started" {
                        "wait_started"
                    } else {
                        "run_finished"
                    }
                && tree[2]["latest_event"]
                    == if root_state == "wait_started" {
                        "wait_started"
                    } else {
                        "run_failed"
                    }
            {
                // The parent also waits for status before it waits specifically for completion.
                if root_state != "wait_started"
                    || client
                        .get(format!(
                            "http://{address}/runs/{}/messages",
                            tree[0]["agent_id"]
                        ))
                        .send()
                        .await
                        .unwrap()
                        .json::<Vec<Value>>()
                        .await
                        .unwrap()
                        .iter()
                        .any(|m| m["payload"] == "working" && m["consumed_sequence"].is_number())
                {
                    return tree;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("dynamic workflow reaches the expected state")
}
