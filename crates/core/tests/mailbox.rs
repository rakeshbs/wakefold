use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use futures_executor::block_on;
use serde_json::json;
use wakefold::{
    CancellationToken, ClientMessageError, JournalError, JsonValue, Message, ModelRequest,
    ModelResponse, Provider, ProviderError, RunError, RunEvent, RunOptions, RunOutcome,
    ScheduledRunConfig, SendMessage, Skill, Tool, ToolCall, WaitForMessage, WaitOn, WakeEvent,
    WorkspaceStore,
};

const CHAT: wakefold::ChatId = wakefold::ChatId::from_u128(1);

const PROMPT: &str = "Exchange the notebook price.";
const CHILD_STATE: &str = "WAKEFOLD_MAILBOX_TEST_STATE";
static TOOLS: [&dyn Tool; 2] = [&SendMessage, &WaitForMessage];
fn mailbox_skills() -> [Skill<'static>; 1] {
    [Skill::new("mailbox", "Send and receive messages", &TOOLS)]
}

#[test]
fn opening_a_legacy_mailbox_keeps_order_and_consumption() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let sender = store.queue_run(CHAT, "sender", "agent", PROMPT).unwrap();
    let receiver = store.queue_run(CHAT, "receiver", "agent", PROMPT).unwrap();
    drop(store);

    let db = rusqlite::Connection::open(state.path().join("wakefold.sqlite3")).unwrap();
    db.execute_batch(
        "DROP TABLE mailbox;
         CREATE TABLE mailbox (
             arrival_order INTEGER PRIMARY KEY,
             message_id TEXT NOT NULL UNIQUE,
             from_agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id),
             to_agent_id INTEGER NOT NULL REFERENCES workspace_runs(agent_id),
             payload_json TEXT NOT NULL,
             consumed_sequence INTEGER
         );
         CREATE INDEX unread_mailbox ON mailbox(to_agent_id, arrival_order)
         WHERE consumed_sequence IS NULL;",
    )
    .unwrap();
    db.execute(
        "INSERT INTO mailbox VALUES (4, 'old-consumed', ?1, ?2, '1', 7),
                                    (8, 'old-unread', ?1, ?2, '2', NULL)",
        [sender, receiver],
    )
    .unwrap();

    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let posted = store
        .post_client_message(receiver, "client-1", &json!(3))
        .unwrap();
    let mut all = db
        .prepare(
            "SELECT arrival_order, message_id, from_agent_id, consumed_sequence
         FROM mailbox ORDER BY arrival_order",
        )
        .unwrap();
    let rows: Vec<(i64, String, Option<i64>, Option<i64>)> = all
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            (4, "old-consumed".into(), Some(sender), Some(7)),
            (8, "old-unread".into(), Some(sender), None),
            (9, posted, None, None),
        ]
    );
}

#[test]
fn retrying_an_accepted_client_message_after_the_run_ends_returns_the_saved_id() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let id = store.queue_run(CHAT, "receiver", "agent", PROMPT).unwrap();
    let payload = json!({"price_cents": 350});
    let posted = store.post_client_message(id, "client-1", &payload).unwrap();
    assert!(
        store
            .open_journal(CHAT, "receiver")
            .unwrap()
            .record_failure("stopped".into())
            .unwrap()
    );
    assert_eq!(
        store.post_client_message(id, "client-1", &payload).unwrap(),
        posted
    );
    assert!(matches!(
        store.post_client_message(id, "client-1", &json!(0)),
        Err(ClientMessageError::KeyConflict)
    ));
    assert!(matches!(
        store.post_client_message(id, "client-2", &payload),
        Err(ClientMessageError::RunEnded)
    ));
    assert!(matches!(
        store.post_client_message(id + 1, "client-2", &payload),
        Err(ClientMessageError::UnknownRun)
    ));
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
    chat: wakefold::ChatId,
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
        CHAT,
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
        .open_run_workspace(CHAT, "receiver")
        .unwrap()
        .agent_id();
    let result = run(
        &mut store,
        CHAT,
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
        .open_run_workspace(CHAT, "receiver")
        .unwrap()
        .agent_id();
    let sender = store.open_run_workspace(CHAT, "sender").unwrap().agent_id();
    assert!(matches!(
        send(&mut store, "sender", recipient).unwrap(),
        RunOutcome::Finished(_)
    ));
    assert!(matches!(run(&mut store, CHAT, "receiver", &Receiver {
        expected_sender: sender, wait_again: true,
    }).unwrap(), RunOutcome::Suspended(wait) if wait.on == WaitOn::MailboxMessage { from_agent_id: None }));

    let sent = store
        .open_journal(CHAT, "sender")
        .unwrap()
        .load_after(0)
        .unwrap();
    let received = store
        .open_journal(CHAT, "receiver")
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
        .open_run_workspace(CHAT, "receiver")
        .unwrap()
        .agent_id();
    let sender = store.open_run_workspace(CHAT, "sender").unwrap().agent_id();
    let receiver = Receiver {
        expected_sender: sender,
        wait_again: false,
    };
    let RunOutcome::Suspended(wait) = run(&mut store, CHAT, "receiver", &receiver).unwrap() else {
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
        run(&mut store, CHAT, "receiver", &receiver).unwrap(),
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
        run(&mut store, CHAT, "receiver", &receiver).unwrap(),
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
        .open_run_workspace(wakefold::ChatId::from_u128(2), "receiver")
        .unwrap()
        .agent_id();
    let result = send(&mut store, "sender", foreign);
    assert!(matches!(result,
        Err(RunError::Journal(JournalError::InvalidMailboxRecipient { agent_id })) if agent_id == foreign));
}
