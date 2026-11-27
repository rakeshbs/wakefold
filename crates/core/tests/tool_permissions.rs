use std::path::Path;

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    JsonValue, Message, ModelRequest, ModelResponse, Provider, ProviderError, RunError, RunOptions,
    RunOutcome, Skill, Tool, ToolCall, ToolError, ToolGrant, ToolSpec, WakeEvent, WorkspaceStore,
};

const CHAT: wakefold::ChatId = wakefold::ChatId::from_u128(1);

const TASK: &str = "Find the notebook price";

struct LookupPrice<'a> {
    path: &'a Path,
    name: &'static str,
}

#[async_trait]
impl Tool for LookupPrice<'_> {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name, "Look up a price", json!({"type": "object"}))
    }

    async fn call(&self, arguments: &JsonValue) -> Result<JsonValue, ToolError> {
        assert_eq!(*arguments, json!({"sku": "notebook"}));
        std::fs::File::create_new(self.path.join(self.name)).unwrap();
        Ok(json!("350 cents"))
    }
}

struct Researcher {
    tool: &'static str,
    expected: JsonValue,
}

#[async_trait]
impl Provider for Researcher {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = match request.messages.last().unwrap() {
            Message::User(prompt) => {
                assert_eq!(prompt, TASK);
                assert!(!request.tools[0].description.contains("admin:"));
                ToolCall::new("load", "load_skill", json!({"name": "prices"})).into()
            }
            Message::Tool(result) if result.call_id == "load".into() => {
                let tools = result.value["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0]["name"], "prices.lookup");
                ToolCall::new(
                    "price",
                    "call_tool",
                    json!({"tool_name": self.tool, "arguments": {"sku": "notebook"}}),
                )
                .into()
            }
            Message::Tool(result) => {
                assert_eq!(result.call_id, "price".into());
                assert_eq!(result.value, self.expected);
                ModelResponse::Final(result.value.as_str().unwrap_or("declined").into())
            }
            other => panic!("unexpected researcher boundary {other:?}"),
        };
        Ok(response)
    }
}

fn research_options(grant: ToolGrant) -> RunOptions {
    RunOptions::new(3).with_tool_grants([("prices.lookup", grant)])
}

#[test]
fn ungranted_tool_is_hidden_and_cannot_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let lookup = LookupPrice {
        path: root.path(),
        name: "lookup",
    };
    let private = LookupPrice {
        path: root.path(),
        name: "private",
    };
    let tools: [&dyn Tool; 2] = [&lookup, &private];
    let skills = [
        Skill::new("prices", "Prices", &tools),
        Skill::new("admin", "Restricted tools", &tools),
    ];
    let denied = block_on(wakefold::run_with_options(
        TASK,
        &Researcher {
            tool: "prices.private",
            expected: JsonValue::Null,
        },
        &skills,
        research_options(ToolGrant::Allow),
    ));
    assert!(matches!(denied, Err(RunError::ToolAccessDenied { name }) if name == "prices.private"));
    assert!(!root.path().join("private").exists());
}

#[test]
fn declined_approval_finishes_the_call_without_running_the_handler() {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(root.path()).unwrap();
    let lookup = LookupPrice {
        path: root.path(),
        name: "lookup",
    };
    let tools: [&dyn Tool; 1] = [&lookup];
    let skills = [Skill::new("prices", "Prices", &tools)];
    let provider = Researcher {
        tool: "prices.lookup",
        expected: json!({"approved": false}),
    };
    let pending = block_on(store.open_journal(CHAT, "run").unwrap().run_until_blocked(
        TASK,
        &provider,
        &skills,
        research_options(ToolGrant::RequireApproval),
    ))
    .unwrap();
    let RunOutcome::Suspended(wait) = pending else {
        panic!("expected an approval request")
    };
    assert!(
        store
            .resolve_wait(&wait.id, WakeEvent::Approval { approved: false })
            .unwrap()
    );
    assert!(
        !store
            .resolve_wait(&wait.id, WakeEvent::Approval { approved: true })
            .unwrap()
    );
    let outcome = block_on(store.open_journal(CHAT, "run").unwrap().run_until_blocked(
        TASK,
        &provider,
        &skills,
        research_options(ToolGrant::RequireApproval),
    ))
    .unwrap();
    assert!(matches!(outcome, RunOutcome::Finished(report) if report.answer == "declined"));
    assert!(!root.path().join("lookup").exists());
}
