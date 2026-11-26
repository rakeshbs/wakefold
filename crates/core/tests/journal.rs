use std::{sync::Barrier, thread};

use serde_json::json;
use wakefold::{
    JournalError, ModelResponse, ModelTurn, RunEvent, TokenUsage, ToolCall, ToolRecovery,
    ToolResult, WorkspaceStore,
};

#[test]
fn stored_text_only_tool_response_decodes_as_a_final_answer() {
    let turn: ModelTurn = serde_json::from_value(json!({
        "response": {"ToolCalls": {"text": "answer", "calls": []}},
        "usage": {"input_tokens": 3, "output_tokens": 1},
        "context": {"trace": 7}
    }))
    .unwrap();
    assert_eq!(turn.response, ModelResponse::Final("answer".into()));
    assert_eq!(turn.usage, Some(TokenUsage::new(3, 1)));
    assert_eq!(turn.replay_data, Some(json!({"trace": 7})));
}

#[test]
fn reopens_run_events_in_order_with_provider_replay_data() {
    let state = tempfile::tempdir().unwrap();
    let call = ToolCall::new("call-1", "lookup", json!({"id": 3}));
    let mut turn = ModelTurn::new(
        ModelResponse::from(call.clone()),
        Some(TokenUsage::new(4, 2)),
    );
    turn.replay_data = Some(json!({"opaque": ["provider data"]}));
    let events = [
        RunEvent::RunStarted("hello".into()),
        RunEvent::ModelCallRequested,
        RunEvent::ModelCallCompleted(turn),
        RunEvent::ToolCallPrepared {
            call,
            recovery: ToolRecovery::RetryAfterCrash,
            tool_call_key: "saved-key".into(),
        },
        RunEvent::ToolCallCompleted(ToolResult::new("call-1", json!(42))),
        RunEvent::ModelCallRequested,
        RunEvent::ModelCallCompleted(ModelTurn::new(ModelResponse::Final("42".into()), None)),
        RunEvent::RunFinished("42".into()),
    ];
    let model_event = serde_json::to_value(&events[2]).unwrap();
    assert_eq!(
        model_event["data"]["context"],
        json!({"opaque": ["provider data"]})
    );
    let prepared_event = serde_json::to_value(&events[3]).unwrap();
    assert_eq!(prepared_event["kind"], "tool_call_started");
    assert_eq!(prepared_event["data"]["idempotency_key"], "saved-key");
    {
        let mut store = WorkspaceStore::open(state.path()).unwrap();
        let mut journal = store.open_journal("chat", "run").unwrap();
        assert_eq!(journal.append(0, &events[..4]).unwrap(), 4);
        assert_eq!(journal.append(4, &events[4..]).unwrap(), 8);
    }
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let journal = store.open_journal("chat", "run").unwrap();
    let entries = journal.load_after(0).unwrap();
    assert_eq!(entries.len(), events.len());
    for (index, (entry, expected)) in entries.iter().zip(&events).enumerate() {
        assert_eq!(entry.sequence, index as u64 + 1);
        assert_eq!(&entry.event, expected);
    }
    assert_eq!(journal.load_after(4).unwrap(), entries[4..]);
    assert!(journal.load_after(8).unwrap().is_empty());
    for (chat, run) in [("other", "run"), ("chat", "other")] {
        assert!(
            store
                .open_journal(chat, run)
                .unwrap()
                .load_after(0)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn concurrent_writers_at_the_same_sequence_commit_only_one_batch() {
    let state = tempfile::tempdir().unwrap();
    let start = Barrier::new(2);
    let writer = |prompt: &str| {
        let mut store = WorkspaceStore::open(state.path()).unwrap();
        let mut journal = store.open_journal("chat", "run").unwrap();
        start.wait();
        journal.append(
            0,
            &[
                RunEvent::RunStarted(prompt.into()),
                RunEvent::ModelCallRequested,
            ],
        )
    };
    let (a, b) = thread::scope(|scope| {
        let worker = scope.spawn(|| writer("first"));
        let b = writer("second");
        (worker.join().unwrap(), b)
    });
    let (winner, loser, prompt) = if a.is_ok() {
        (a, b, "first")
    } else {
        (b, a, "second")
    };
    assert_eq!(winner.unwrap(), 2);
    assert!(matches!(
        loser,
        Err(JournalError::SequenceMismatch {
            expected: 0,
            actual: 2
        })
    ));
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    let entries = journal.load_after(0).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].event, RunEvent::RunStarted(prompt.into()));
    assert_eq!(
        journal
            .append(2, &[RunEvent::RunFinished("done".into())])
            .unwrap(),
        3
    );
}

#[test]
fn interrupted_batch_rolls_back_all_its_entries() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let mut journal = store.open_journal("chat", "run").unwrap();
    let db = rusqlite::Connection::open(state.path().join("wakefold.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_insert BEFORE INSERT ON run_events
        WHEN NEW.sequence = 2 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    let batch = [
        RunEvent::RunStarted("hello".into()),
        RunEvent::ModelCallRequested,
    ];
    assert!(matches!(
        journal.append(0, &batch),
        Err(JournalError::Database(_))
    ));
    assert!(journal.load_after(0).unwrap().is_empty());
    db.execute_batch("DROP TRIGGER fail_insert").unwrap();
    assert_eq!(journal.append(0, &batch).unwrap(), 2);
}

#[test]
fn existing_run_gains_an_empty_journal_without_changing_its_workspace() {
    let state = tempfile::tempdir().unwrap();
    let run = WorkspaceStore::open(state.path())
        .unwrap()
        .open_run_workspace("chat", "old")
        .unwrap();
    let file = run.chat_root().join("note");
    std::fs::write(&file, "existing data").unwrap();
    // Reproduce the schema before journal support, preserving its real run metadata.
    rusqlite::Connection::open(state.path().join("wakefold.sqlite3"))
        .unwrap()
        .execute_batch("DROP TABLE run_events")
        .unwrap();

    let mut store = WorkspaceStore::open(state.path()).unwrap();
    assert_eq!(store.open_run_workspace("chat", "old").unwrap(), run);
    let mut journal = store.open_journal("chat", "old").unwrap();
    assert!(journal.load_after(0).unwrap().is_empty());
    assert_eq!(
        journal
            .append(0, &[RunEvent::RunStarted("resume".into())])
            .unwrap(),
        1
    );
    assert_eq!(std::fs::read_to_string(file).unwrap(), "existing data");
}
