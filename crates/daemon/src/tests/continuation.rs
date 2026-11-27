use super::*;
use wakefold::ToolContext;

const TEST: &str = "tests::continuation::follow_up_keeps_the_agent_and_survives_restart";
const FIRST: &str = "Write the field report.";
const SECOND: &str = "Wait for my correction, then update the report.";
const FIRST_ANSWER: &str = "I wrote the field report.";
const SECOND_ANSWER: &str = "I updated the field report.";

struct ReportProvider;

#[async_trait]
impl Provider for ReportProvider {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let suffix = request
            .request_context
            .expect("request includes today's date");
        assert!(suffix.starts_with("Current date: "));
        assert!(suffix.ends_with("(Asia/Kolkata)."));
        let follow_up = request
            .messages
            .iter()
            .any(|message| matches!(message, Message::User(text) if text == SECOND));
        if follow_up {
            assert!(request.messages.iter().any(|message|
                matches!(message, Message::Assistant(ModelResponse::Final(text)) if text == FIRST_ANSWER)
            ));
            assert!(request.messages.iter().any(|message|
                matches!(message, Message::Tool(result) if result.call_id.as_str() == "write"
                    && result.value == "report saved")
            ));
        }
        let load = |id, name| ToolCall::new(id, "load_skill", json!({"name":name})).into();
        Ok(match request.messages.last().unwrap() {
            Message::Developer(_) => load("notes", "notes"),
            Message::Tool(result) if result.call_id.as_str() == "notes" => {
                dispatch("write", "notes.save", json!({"text":"first version"})).into()
            }
            Message::Tool(result) if result.call_id.as_str() == "write" => load("mail", "mailbox"),
            Message::Tool(result) if result.call_id.as_str() == "mail" => {
                ModelResponse::Final(FIRST_ANSWER.into())
            }
            Message::User(text) if text == SECOND => {
                dispatch("correction", "mailbox.receive", json!({})).into()
            }
            Message::Tool(result) if result.call_id.as_str() == "correction" => {
                assert_eq!(result.value["payload"], "corrected version");
                dispatch(
                    "edit",
                    "notes.save",
                    json!({"text":result.value["payload"]}),
                )
                .into()
            }
            Message::Tool(result) if result.call_id.as_str() == "edit" => {
                assert_eq!(result.value, "report saved");
                ModelResponse::Final(SECOND_ANSWER.into())
            }
            other => panic!("unexpected conversation boundary: {other:?}"),
        })
    }

    async fn stream_turn(
        &self,
        request: ModelRequest<'_>,
        _: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<ModelTurn, ProviderError> {
        Ok(ModelTurn::new(
            self.complete_response(request).await?,
            Some(TokenUsage::new(3, 1)),
        ))
    }
}

struct SaveReport;

#[async_trait]
impl Tool for SaveReport {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("save", "Write the field report", json!({"type":"object"}))
    }

    async fn call(&self, _: &Value) -> Result<Value, ToolError> {
        unreachable!("reports are stored in the agent workspace")
    }

    async fn call_with_context(
        &self,
        context: ToolContext,
        args: &Value,
    ) -> Result<Value, ToolError> {
        let workspace = context.workspace().unwrap();
        let path = workspace
            .chat_root()
            .join(workspace.agent_relative_directory())
            .join("report.txt");
        let text = args["text"].as_str().unwrap();
        if text == "corrected version" {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "first version");
        }
        std::fs::write(path, text).unwrap();
        Ok(json!("report saved"))
    }
}

#[tokio::test]
async fn follow_up_keeps_the_agent_and_survives_restart() {
    if let Some(path) = std::env::var_os(CHILD_CONFIG) {
        let path = PathBuf::from(path);
        let config = Config::load(&path).unwrap();
        let skills = BTreeMap::from([(
            "notes".into(),
            RegisteredSkill {
                description: "Write reports".into(),
                tools: vec![Box::new(SaveReport)],
            },
        )]);
        let runtime = Runtime::build(config, skills, |_| Ok(Box::new(ReportProvider)))
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
    tokio::time::timeout(Duration::from_secs(30), async {
        let state = tempfile::tempdir().unwrap();
        let config = state.path().join("config.json");
        std::fs::write(
            &config,
            json!({
                "state_directory":"state","bind":"127.0.0.1:0",
                "defaults":{"model":"report-test","max_model_turns":4,"token_budget":16}
            })
            .to_string(),
        )
        .unwrap();
        let client = Client::builder().no_proxy().build().unwrap();
        let (daemon, address) = launch(&config, TEST).await;
        let chat = wakefold::ChatId::new_v4();
        let first =
            json!({"chat_id":chat,"run_key":"write","prompt":FIRST,"time_zone":"Asia/Kolkata"});
        let started = submit(&client, address, &first).await;
        let first_id = started["run_id"].as_i64().unwrap();
        assert_eq!(
            until_event(address, first_id, "run_finished").await["event"]["data"],
            FIRST_ANSWER
        );
        let earlier = history(&client, address, chat).await;

        let second = json!({"chat_id":chat,"run_key":"edit","prompt":SECOND});
        let continued = submit(&client, address, &second).await;
        assert_eq!(continued["agent_id"], started["agent_id"]);
        let second_id = continued["run_id"].as_i64().unwrap();
        assert_ne!(first_id, second_id);
        let waiting = until_event(address, second_id, "wait_started").await;
        let busy = client
            .post(format!("http://{address}/runs"))
            .json(&json!({
                "chat_id":chat,"run_key":"third","prompt":"An overlapping request."
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(busy.status(), StatusCode::CONFLICT);
        assert_eq!(submit(&client, address, &second).await, continued);
        drop(daemon);

        let (_daemon, address) = launch(&config, TEST).await;
        assert_eq!(submit(&client, address, &second).await, continued);
        assert_eq!(
            until_event(address, second_id, "wait_started").await,
            waiting
        );
        let signal = json!({"message_key":"correction","payload":"corrected version"});
        let url = format!("http://{address}/runs/{second_id}/messages");
        let send = || async {
            client
                .post(&url)
                .json(&signal)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        };
        assert_eq!(send().await, send().await);
        assert_eq!(
            until_event(address, second_id, "run_finished").await["event"]["data"],
            SECOND_ANSWER
        );

        let events = history(&client, address, chat).await;
        assert_eq!(&events[..earlier.len()], earlier.as_slice());
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence"], index + 1);
            assert!(!event.to_string().contains("Current date:"));
            assert_eq!(
                event["run_id"],
                if index < earlier.len() {
                    first_id
                } else {
                    second_id
                }
            );
        }
        let prompts: Vec<_> = events
            .iter()
            .filter(|e| e["event"]["kind"] == "run_started")
            .map(|e| e["event"]["data"].as_str().unwrap())
            .collect();
        assert_eq!(prompts, [FIRST, SECOND]);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"]["kind"] == "woken")
                .count(),
            1
        );
        let folder = state
            .path()
            .join("state/workspaces")
            .join(chat.to_string())
            .join("agents");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
        let agent = started["agent_id"].as_i64().unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join(agent.to_string()).join("report.txt")).unwrap(),
            "corrected version"
        );
    })
    .await
    .expect("both turns finish across the restart");
}

async fn submit(client: &Client, address: SocketAddr, request: &Value) -> Value {
    client
        .post(format!("http://{address}/runs"))
        .json(request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn history(client: &Client, address: SocketAddr, chat: wakefold::ChatId) -> Vec<Value> {
    client
        .get(format!("http://{address}/chats/{chat}/history"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn until_event(address: SocketAddr, run: i64, kind: &str) -> Value {
    let (mut socket, _) = connect_async(format!("ws://{address}/runs/{run}/events"))
        .await
        .unwrap();
    loop {
        let event = next_event(&mut socket).await;
        assert_eq!(event["run_id"], run);
        if event["event"]["kind"] == kind {
            return event;
        }
        assert_ne!(event["event"]["kind"], "run_failed", "{event}");
    }
}
