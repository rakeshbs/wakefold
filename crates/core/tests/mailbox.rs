use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    CancellationToken, JournalError, JsonValue, Message, ModelRequest, ModelResponse, Provider,
    ProviderError, RunError, RunEvent, RunOptions, RunOutcome, ScheduledRunConfig, SendMessage,
    Skill, Tool, ToolCall, WaitForMessage, WaitOn, WakeEvent, WorkspaceStore,
};

const PROMPT: &str = "Exchange the notebook price.";
const CHILD_STATE: &str = "WAKEFOLD_MAILBOX_TEST_STATE";
static TOOLS: [&dyn Tool; 2] = [&SendMessage, &WaitForMessage];
fn mailbox_skills() -> [Skill<'static>; 1] {
    [Skill::new("mailbox", "Send and receive messages", &TOOLS)]
}

fn mailbox_call(id: &str, tool_name: &str, arguments: JsonValue) -> ModelResponse {
    ToolCall::new(
        id,
        "call_tool",
        json!({ "tool_name": tool_name, "arguments": arguments }),
    )
    .into()
}

fn load_mailbox() -> ModelResponse {
    ToolCall::new("load", "load_skill", json!({ "name": "mailbox" })).into()
}

struct Sender {
    recipient: i64,
    exit_after_send: bool,
}

#[async_trait]
impl Provider for Sender {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        Ok(match request.messages.last().unwrap() {
            Message::User(_) => load_mailbox(),
            Message::Tool(result) if result.call_id == "load".into() => mailbox_call(
                "send",
                "mailbox.send",
                json!({"to_agent_id": self.recipient, "payload": {"price_cents": 350}}),
            ),
            Message::Tool(result) => {
                assert_eq!(result.call_id, "send".into());
                assert_eq!(result.value["to_agent_id"], self.recipient);
                assert!(result.value["message_id"].is_string());
                if self.exit_after_send {
                    std::process::exit(76);
                }
                ModelResponse::Final("sent".into())
            }
            other => panic!("unexpected sender boundary {other:?}"),
        })
    }
}

struct Receiver {
    expected_sender: i64,
    wait_again: bool,
}

#[async_trait]
impl Provider for Receiver {
    async fn complete_response(
        &self,
        request: ModelRequest<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        Ok(match request.messages.last().unwrap() {
            Message::User(_) => load_mailbox(),
            Message::Tool(result) if result.call_id == "load".into() => mailbox_call(
                "receive",
                "mailbox.receive",
                json!({ "from_agent_id": self.expected_sender }),
            ),
            Message::Tool(result) => {
                assert_eq!(
                    result.call_id,
                    "receive".into(),
                    "the message must not be delivered twice"
                );
                assert_eq!(result.value["from_agent_id"], self.expected_sender);
                assert_eq!(result.value["payload"], json!({ "price_cents": 350 }));
                assert!(result.value["message_id"].is_string());
                if self.wait_again {
                    mailbox_call("receive-again", "mailbox.receive", json!({}))
                } else {
                    ModelResponse::Final("350 cents".into())
                }
            }
            other => panic!("unexpected receiver boundary {other:?}"),
        })
    }
}

fn run(
    store: &mut WorkspaceStore,
    chat: &str,
    key: &str,
    provider: &dyn Provider,
) -> Result<RunOutcome, RunError> {
    block_on(store.open_journal(chat, key).unwrap().run_until_blocked(
        PROMPT,
        provider,
        &mailbox_skills(),
        RunOptions::new(5),
    ))
}

fn send(store: &mut WorkspaceStore, key: &str, recipient: i64) -> Result<RunOutcome, RunError> {
    run(
        store,
        "chat",
        key,
        &Sender {
            recipient,
            exit_after_send: false,
        },
    )
}

fn send_and_exit(path: &Path) {
    let mut store = WorkspaceStore::open(path).unwrap();
    let recipient = store
        .open_run_workspace("chat", "receiver")
        .unwrap()
        .agent_id();
    let result = run(
        &mut store,
        "chat",
        "sender",
        &Sender {
            recipient,
            exit_after_send: true,
        },
    );
    panic!("expected process exit after committed send, got {result:?}");
}

#[test]
fn message_sent_before_the_wait_survives_restart_and_is_consumed_once() {
    if let Some(path) = std::env::var_os(CHILD_STATE) {
        send_and_exit(Path::new(&path));
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "message_sent_before_the_wait_survives_restart_and_is_consumed_once",
        ])
        .env(CHILD_STATE, state.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(76));

    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let recipient = store
        .open_run_workspace("chat", "receiver")
        .unwrap()
        .agent_id();
    let sender = store
        .open_run_workspace("chat", "sender")
        .unwrap()
        .agent_id();
    assert!(matches!(
        send(&mut store, "sender", recipient).unwrap(),
        RunOutcome::Finished(_)
    ));
    assert!(matches!(run(&mut store, "chat", "receiver", &Receiver {
        expected_sender: sender, wait_again: true,
    }).unwrap(), RunOutcome::Suspended(wait) if wait.on == WaitOn::MailboxMessage { from_agent_id: None }));

    let sent = store
        .open_journal("chat", "sender")
        .unwrap()
        .load_after(0)
        .unwrap();
    let received = store
        .open_journal("chat", "receiver")
        .unwrap()
        .load_after(0)
        .unwrap();
    let mut sent_ids = sent.iter().filter_map(|entry| match &entry.event {
        RunEvent::MailboxMessageSent { message_id, .. } => Some(message_id.as_str()),
        _ => None,
    });
    let sent_id = sent_ids.next().unwrap();
    assert!(sent_ids.next().is_none());
    let received_ids: Vec<_> = received
        .iter()
        .filter_map(|entry| match &entry.event {
            RunEvent::Woken { payload, .. } => payload["message_id"].as_str(),
            _ => None,
        })
        .collect();
    assert_eq!(received_ids, [sent_id]);
}

#[test]
fn a_matching_send_wakes_the_receiver_and_rolls_back_if_the_wake_fails() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let recipient = store
        .open_run_workspace("chat", "receiver")
        .unwrap()
        .agent_id();
    let sender = store
        .open_run_workspace("chat", "sender")
        .unwrap()
        .agent_id();
    let receiver = Receiver {
        expected_sender: sender,
        wait_again: false,
    };
    let RunOutcome::Suspended(wait) = run(&mut store, "chat", "receiver", &receiver).unwrap()
    else {
        panic!("receiver should wait for the sender")
    };

    assert!(
        !store
            .resolve_wait(&wait.id, WakeEvent::Message(json!("spoofed")))
            .unwrap()
    );
    assert!(matches!(
        send(&mut store, "other-sender", recipient).unwrap(),
        RunOutcome::Finished(_)
    ));
    assert_eq!(
        run(&mut store, "chat", "receiver", &receiver).unwrap(),
        RunOutcome::Suspended(wait.clone())
    );

    let db = rusqlite::Connection::open(state.path().join("wakefold.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_mailbox_wake BEFORE INSERT ON run_events
         WHEN json_extract(NEW.event_json, '$.kind') = 'woken'
         BEGIN SELECT RAISE(ABORT, 'interrupt mailbox wake'); END;",
    )
    .unwrap();
    assert!(matches!(
        send(&mut store, "sender", recipient),
        Err(RunError::Journal(_))
    ));
    assert_eq!(
        run(&mut store, "chat", "receiver", &receiver).unwrap(),
        RunOutcome::Suspended(wait)
    );
    let sender_rows: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM mailbox WHERE from_agent_id = ?1",
            [sender],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sender_rows, 0,
        "a failed wake must roll back the queued send"
    );
    db.execute_batch("DROP TRIGGER fail_mailbox_wake").unwrap();
    assert!(matches!(
        send(&mut store, "sender", recipient).unwrap(),
        RunOutcome::Finished(_)
    ));
    drop(store);

    let token = CancellationToken::new();
    let skills = mailbox_skills();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5),
            WorkspaceStore::open(state.path()).unwrap().run_scheduler(
                NonZeroUsize::MIN,
                &token,
                |run| {
                    assert_eq!(run.agent_id, recipient);
                    ScheduledRunConfig::new(&receiver, &skills, RunOptions::new(5))
                },
                |_, outcome| {
                    assert!(matches!(outcome, RunOutcome::Finished(report) if report.answer == "350 cents"));
                    token.cancel();
                },
            ),
        ).await.unwrap().unwrap();
    });
}

#[test]
fn a_sender_cannot_address_a_recipient_in_another_chat() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let foreign = store
        .open_run_workspace("other-chat", "receiver")
        .unwrap()
        .agent_id();
    let result = send(&mut store, "sender", foreign);
    assert!(matches!(result,
        Err(RunError::Journal(JournalError::InvalidMailboxRecipient { agent_id })) if agent_id == foreign));
}
