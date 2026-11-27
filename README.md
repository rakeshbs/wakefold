# Wakefold

Wakefold is a Rust library and local daemon for AI agents that can pause and resume across restarts. It records each run in SQLite so you can replay what happened.

Agents can wait for a timer, a message, or human approval. They can delegate work to other agents with their own tool permissions and token budgets, load skills when needed, and call tools from local Model Context Protocol (MCP) servers.

## Chat workspaces

`POST /chats` creates a chat and returns its `chat_id` UUID. Send that UUID
with `POST /runs` and artifact requests. Clients can also generate a UUID and
use it in their first run request. Wakefold stores the chat's working files in
`workspaces/<chat_id>/agents/<agent_id>/` under the daemon's state directory.
The same IDs reopen the workspace after a restart.

On upgrade from the numeric workspace layout, Wakefold moves existing chat
folders to UUID paths and saves the previous keys and their UUIDs in
`chat-id-migration.json` in the state directory. Use the mapped UUID as
`chat_id` in requests for an existing chat.

## Dynamic agents

A daemon can run roots and subagents from one shared configuration. For example:

```json
{
  "state_directory": "state",
  "defaults": {
    "model": "gpt-5.4",
    "instructions": "Use the mailbox to coordinate research.",
    "max_model_turns": 30,
    "token_budget": 40000,
    "tool_grants": { "artifacts.publish": "require_approval" }
  }
}
```

Send `chat_id`, `run_key`, and `prompt` to `POST /runs` to start a root.
The first root can also send an IANA `time_zone` such as `Asia/Kolkata`.
It defaults to UTC. Each run saves its start date, instructions, and skill
descriptions. The root loads the `agents` skill and dispatches `agents.spawn`
with a `task`. The result contains the child's `agent_id`. Each child starts
with its own journal, workspace folder, and full per-agent token budget.

Shared defaults expose installed skills on demand. Tools allow execution by
default, and `tool_grants` can require approval for individual tools. Only
roots can load the spawning skill. Agents can load `mailbox` and use:

- `mailbox.send` with `to_agent_id` and a JSON `payload`.
- `mailbox.receive` with optional `from_agent_id`. It waits by default.
  Use `wait: false` to check immediately. An empty check returns `null`.
- `completion_only: true` on a receive to select a child's final answer or
  failure. Progress messages stay available for a later receive.

`GET /chats/{chat_id}/runs` lists the run tree. `GET /runs/{agent_id}/messages`
returns up to 128 incoming messages. Pass the last `arrival_order` as `after`
for the next page. The existing events WebSocket streams each run's journal.

## Artifacts

Enable the built-in `artifacts` skill for an agent and grant `artifacts.publish` with `allow` or `require_approval`. The tool publishes an existing file by title and chat-relative `path`. Pass its returned `artifact_id` with a later publication to add a revision.

- `GET /artifacts?chat_id=…` lists each artifact's latest revision.
- `GET /artifacts/{artifact_id}/revisions/{revision}?chat_id=…` downloads that exact revision.

Published copies survive restarts and workspace edits. The run's event stream records the revision it delivered.

## Per-agent sandboxing

Build the daemon with `--features sandbox`. On Linux, the `bubblewrap`
backend requires Bubblewrap, Python 3, libseccomp, and a systemd user
session with cgroup v2 memory and process controllers. On Apple silicon
with macOS 26 or later, start Apple's `container` service and build the
guest image:

```sh
container build -t wakefold-sandbox -f crates/daemon/src/sandbox/Containerfile crates/daemon/src/sandbox
```

Set the daemon's top-level `sandbox` configuration. Agents can read peer
folders in the same chat by default. An optional `read_peers` entry limits
one agent to the listed peers. An empty list gives that agent access only to
its own folder. This example limits agent 12 to peer 7:

```json
{
  "backend": { "kind": "apple_container", "image": "wakefold-sandbox" },
  "memory_mib": 256,
  "max_processes": 32,
  "read_peers": { "12": [7] },
  "idle_seconds": 30,
  "call_timeout_seconds": 30
}
```

On Linux, use `"backend": { "kind": "bubblewrap" }`. The Apple backend also
accepts an `executable` path and a `vm_memory_mib` cap for the entire chat VM.
The defaults are `container` on PATH and 1024 MiB.

Add a `sandbox_command` tool to a skill using the same agent skill list and
tool grants as other tools. The daemon sends the model's JSON arguments on
stdin and expects one JSON value on stdout. Tool commands run with their
agent's folder as the working directory.

```json
{
  "kind": "sandbox_command",
  "name": "report",
  "description": "Save a note and return its filename",
  "command": "/usr/bin/python3",
  "args": ["-I", "-c", "import json, pathlib, sys; data = json.load(sys.stdin); pathlib.Path('note.txt').write_text(data['text']); print(json.dumps({'filename': 'note.txt'}))"],
  "parameters": {
    "type": "object",
    "properties": { "text": { "type": "string" } },
    "required": ["text"],
    "additionalProperties": false
  }
}
```

Each agent gets the same folder permissions across calls. Its own folder is
writable, and the sandbox mounts its readable peer folders read-only. With
the default shared mount, a running sandbox sees new peer folders as they
appear. The sandbox has no external network. Apple `container` provides the
per-chat VM and Bubblewrap applies the per-agent mounts inside it. Linux runs
Bubblewrap directly. Existing in-process and configured MCP tools run as
trusted host tools.

Run the real backend acceptance test with
`cargo test -p wakefold-daemon --features sandbox workspace_permissions_and_limits_survive_reuse -- --ignored`.
On macOS, set `WAKEFOLD_SANDBOX_TEST_IMAGE` to the guest image tag. Set
`WAKEFOLD_SANDBOX_TEST_CONTAINER` when using a custom CLI path.
