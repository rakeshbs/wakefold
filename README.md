# Wakefold

Wakefold is a durable agent harness for Rust.
Its goal is to let agents resume after a crash and wait for events without keeping a task in memory.
The design uses an append-only journal to rebuild execution state and replay each step.

## Suspending a run

Add `TimerWait` to a skill and use `RunJournal::run_until_blocked` to start
the run. When a tool asks to wait, the call returns `RunOutcome::Suspended`
and Wakefold saves the wait in SQLite. `WorkspaceStore::run_scheduler`
resumes the run when its timer is due. The host supplies the provider,
skills and run limits when the scheduler restores it.

Host tools can also return a wait for a message, webhook or approval.
Deliver one of those events through `WorkspaceStore::resolve_wait` with
the saved wait ID. The event becomes the result of the suspended tool call.

## Timer restart benchmark

In one macOS arm64 debug run with a scripted provider, Wakefold registered
1,000 timers in 0.71 seconds. Resident memory was 4.6 MiB before registration
and 8.4 MiB while the scheduler idled with all timers pending. After the
first process exited and the deadlines passed, a fresh process resumed and
finished all 1,000 runs in 0.71 seconds. The memory figures include SQLite
and the test process.

```sh
cargo test --locked -p wakefold --test suspend a_thousand_timers_survive_restart -- --ignored --nocapture
```
