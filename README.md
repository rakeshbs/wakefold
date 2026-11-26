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

## Delegating to a worker

Use `DelegateTool` to bind a tool in the coordinator's skill catalog to a named
worker. A delegation saves the coordinator's wait and queues the worker in
the same transaction. The scheduler supplies the worker's own configuration
using `ScheduledRun::delegation`. A worker's saved answer returns to the
coordinator as a tool result with `worker` and `answer` fields.

Set an agent's allowed tools with `RunOptions::with_tool_grants` using
skill-qualified names such as `prices.lookup`. Supply the grants when resuming
the run too. A `RequireApproval` grant saves the exact proposed tool name and
arguments before dispatch. Hosts can recover those requests with
`WorkspaceStore::pending_tool_approvals` and deliver a decision with
`WakeEvent::Approval`. A declined call returns `{"approved": false}` to the
model.
