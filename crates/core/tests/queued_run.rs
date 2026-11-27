use std::num::NonZeroUsize;

use futures_executor::block_on;
use wakefold::{
    CancellationToken, JournalReplay, QueueRunError, RunEvent, SchedulerError, WorkspaceStore,
};

const CHAT: wakefold::ChatId = wakefold::ChatId::from_u128(1);

fn settings() -> wakefold::AgentSettings {
    serde_json::from_value(
        serde_json::json!({"model":"scripted","max_model_turns":12,"token_budget":200}),
    )
    .unwrap()
}

#[test]
fn journal_replay_keeps_a_snapshot_while_the_run_advances() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let id = store
        .queue_root(CHAT, "job", "Inspect an order.", &settings(), None)
        .unwrap();
    let replay = JournalReplay::open(state.path()).unwrap();
    let first = replay
        .run_event_page(id, 0, NonZeroUsize::MIN)
        .unwrap()
        .unwrap();
    assert_eq!(first.entries.len(), 1);
    assert!(!first.complete);

    assert!(
        store
            .open_journal(CHAT, "job")
            .unwrap()
            .record_failure("stopped".into())
            .unwrap()
    );
    let old = replay
        .run_event_page(id, 2, NonZeroUsize::MIN)
        .unwrap()
        .unwrap();
    assert!(old.entries.is_empty());
    assert!(!old.complete);
    let fresh = JournalReplay::open(state.path())
        .unwrap()
        .run_event_page(id, 2, NonZeroUsize::MIN)
        .unwrap()
        .unwrap();
    assert!(fresh.complete);
    assert!(matches!(&fresh.entries[0].event, RunEvent::RunFailed(error) if error == "stopped"));
}

#[test]
fn queued_run_retains_its_identity_and_terminal_failure_after_reopening() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let id = store
        .queue_root(CHAT, "job", "Review this order.", &settings(), None)
        .unwrap();
    assert_eq!(
        store
            .queue_root(CHAT, "job", "Review this order.", &settings(), None)
            .unwrap(),
        id
    );
    assert!(matches!(
        store.queue_root(
            CHAT,
            "job",
            "Review this order.",
            &settings(),
            Some(chrono_tz::Asia::Kolkata)
        ),
        Err(QueueRunError::Conflict)
    ));
    assert!(matches!(
        store.queue_root(CHAT, "job", "Another prompt", &settings(), None),
        Err(QueueRunError::Conflict)
    ));
    drop(store);

    let result = block_on(
        WorkspaceStore::open(state.path())
            .unwrap()
            .run_scheduler_with_agent_lookup(
                NonZeroUsize::MIN,
                &CancellationToken::new(),
                |_| None,
                |_, _| panic!("the agent configuration is missing"),
            ),
    );
    let Err(SchedulerError::MissingAgent(run)) = result else {
        panic!("unexpected result {result:?}")
    };
    assert_eq!(run.agent_id, id);
    assert_eq!(run.settings.as_deref(), Some(&settings()));

    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let mut journal = store.open_journal(run.chat_id, &run.run_key).unwrap();
    assert!(journal.record_failure("agent removed".into()).unwrap());
    assert!(!journal.record_failure("duplicate failure".into()).unwrap());
    drop(store);

    let store = WorkspaceStore::open(state.path()).unwrap();
    let first = store
        .run_event_page(id, 0, NonZeroUsize::MIN)
        .unwrap()
        .unwrap();
    assert!(!first.complete, "one event remains beyond this page");
    assert!(
        matches!(&first.entries[0].event, RunEvent::RunStarted(prompt) if prompt == "Review this order.")
    );
    let second = store
        .run_event_page(id, 2, NonZeroUsize::MIN)
        .unwrap()
        .unwrap();
    assert!(second.complete);
    assert!(
        matches!(&second.entries[0].event, RunEvent::RunFailed(error) if error == "agent removed")
    );
    let done = store
        .run_event_page(id, second.entries[0].sequence, NonZeroUsize::MIN)
        .unwrap()
        .unwrap();
    assert!(done.complete && done.entries.is_empty());
}
