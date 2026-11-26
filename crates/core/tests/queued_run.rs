use std::num::NonZeroUsize;

use futures_executor::block_on;
use wakefold::{CancellationToken, QueueRunError, RunEvent, SchedulerError, WorkspaceStore};

#[test]
fn queued_run_retains_its_identity_and_terminal_failure_after_reopening() {
    let state = tempfile::tempdir().unwrap();
    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let id = store
        .queue_run("chat", "job", "coordinator", "Review this order.")
        .unwrap();
    assert_eq!(
        store
            .queue_run("chat", "job", "coordinator", "Review this order.")
            .unwrap(),
        id
    );
    assert!(matches!(
        store.queue_run("chat", "job", "worker", "Review this order."),
        Err(QueueRunError::Conflict)
    ));
    assert!(matches!(
        store.queue_run("chat", "job", "coordinator", "Another prompt"),
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
    assert_eq!(run.agent_name.as_deref(), Some("coordinator"));

    let mut store = WorkspaceStore::open(state.path()).unwrap();
    let mut journal = store.open_journal(&run.chat_key, &run.run_key).unwrap();
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
        .run_event_page(id, first.entries[0].sequence, NonZeroUsize::MIN)
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
