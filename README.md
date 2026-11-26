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

## Sending messages between agents

Add `SendMessage` and `WaitForMessage` to a skill named `mailbox`. Give sending
agents the recipient's ID from `RunWorkspace::agent_id`. A sender in the same
chat can call `mailbox.send` with `to_agent_id` and a JSON `payload`.

`mailbox.receive` returns the oldest matching message with its ID, sender ID,
and payload. It accepts an optional `from_agent_id` filter. When no matching
message is queued, the run suspends. A later matching send wakes it.
Wakefold commits delivery and the receiver's wake together in SQLite.

## Running the daemon

Save a JSON configuration and launch the daemon with `OPENAI_API_KEY` in
its environment. The daemon listens on loopback for local clients. Relative
state paths are resolved alongside the configuration file.

```json
{
  "bind": "127.0.0.1:8700",
  "state_directory": "state",
  "skills": {
    "timers": {
      "description": "Wait for a timer",
      "tools": [{ "kind": "timer_wait" }]
    }
  },
  "agents": {
    "assistant": {
      "model": "gpt-6-luna",
      "max_model_turns": 6,
      "token_budget": 6000,
      "skills": ["timers"],
      "tool_grants": { "timers.wait": "allow" }
    }
  }
}
```

```sh
cargo run --locked -p wakefold-daemon -- serve wakefold.json
```

- `POST /runs` takes `chat_key`, `run_key`, `agent`, and `prompt`. It returns
  `202` with an `agent_id`. Retrying the same request returns the same ID.
- Connect to `ws://127.0.0.1:8700/runs/{agent_id}/events` for journal events
  with `sequence` and `event` fields. Reconnect with `?after=<last sequence>`
  to continue after the last event you received.
- `POST /runs/{agent_id}/messages` takes a client-chosen `message_key` and a
  JSON `payload`. It returns `202` with a `message_id`. Retrying with the same
  key and payload returns the same ID. The agent receives the message through
  `mailbox.receive` with no sender filter and sees `from_agent_id: null`.
  Messages stay queued until received.
- `GET /approvals` lists pending requests. Each response includes the
  `agent_id` and the saved `wait` with its ID and tool arguments.
- `POST /approvals/{wait_id}` accepts `{"approved":true}` or
  `{"approved":false}`. The scheduler resumes the saved run.

A grant can be `allow` or `require_approval`. Other built-in tool kinds are
`send_message`, `wait_for_message`, and `delegate`. A delegate tool takes a
local `name`, a `description`, and the target `agent` from the configuration.

## Replaying a run

```sh
cargo run --locked -p wakefold-daemon -- replay state 1
```

Pass the state directory and the `agent_id` returned by `POST /runs`.
Replay opens a read-only SQLite snapshot and writes one JSON object per event
with its `agent_id`, `sequence`, and `event`. A delegation shows the worker's
journal after the parent's wait and before its result. The command works on
both finished and still-pending runs.
