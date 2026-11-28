# Wakefold

Wakefold is a Rust library and local daemon for AI agents that can pause and resume across restarts. It records each run in SQLite so you can replay what happened.

Agents can wait for a timer, a message, or human approval. They can delegate work to other agents with their own tool permissions and token budgets, load skills when needed, and call tools from local Model Context Protocol (MCP) servers.

## Remote daemon access

The default listener is HTTP on `127.0.0.1:8700`. To serve a desktop client on
another machine, set the daemon's `bind` to its network address and configure
TLS in the same JSON configuration:

```json
{
  "bind": "192.168.1.40:8700",
  "remote": {
    "certificate_chain": "server-chain.pem",
    "private_key": "server-key.pem",
    "token_file": "desktop-token"
  }
}
```

Paths are relative to the configuration file. Use a certificate valid for the
hostname or IP address the desktop will connect to, signed by a CA the client
trusts. Generate a random token on the server, for example with
`(umask 077 && openssl rand -hex 32 > desktop-token)`. Protect the private key
and token file. Restart the daemon after changing either. Remote mode requires
the same bearer token for every HTTPS request and WSS upgrade. Possession of
the token grants access to that daemon's chats and management API.

## Desktop client

Start your configured daemon with `cargo run -p wakefold-daemon -- serve <config.json>`.
Start the native app with `cargo run -p wakefold-desktop`. Select **This Mac**
for the local daemon on port 8700. For a remote daemon, choose **Add server**,
enter its name and HTTPS URL, copy its access token, and click **Paste token**
and **Connect and save**. The app saves server names and addresses in
`Wakefold/servers.json` under the OS configuration directory. Tokens go in the
OS credential store (Keychain on macOS). Add the same URL again to replace
its token. For a private CA, you can set `WAKEFOLD_CA_FILE` to its PEM file
when launching the client.

Select a saved chat and then a run to watch its activity. The client shows
connection status and the latest 200 events. After an interruption it
refreshes the run tree and pending approvals and replays missed events for
the selected run. Launch with `--preview` for the interactive chat design
with sample data.

From the repository root, run `crates/desktop/tests/remote-macos.sh` to test
remote mode locally. It starts a separate daemon with a deterministic model
provider, generates temporary TLS credentials, interrupts the connection
during a run, and compares the desktop event stream with the saved journal.

## Chat workspaces

`POST /chats` creates a chat and returns its `chat_id` UUID. Send that UUID
with `POST /runs` and artifact requests. Clients can also generate a UUID and
use it in their first run request. Wakefold stores the chat's working files in
`workspaces/<chat_id>/agents/<agent_id>/` under the daemon's state directory.
The same IDs reopen the workspace after a restart.

The runtime uses the [six-table SQLite schema](crates/core/src/workspace/schema.sql).
It upgrades the previous seven-table schema in place. If a previous chat has
several independent roots, the newest active root becomes the continuing
agent. If all roots have finished, the newest root becomes the continuing
agent. Each earlier root keeps its own journal and workspace.

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

Send `chat_id`, `run_key`, and `prompt` to `POST /runs`. It returns the stable
root `agent_id` and the request's `run_id`. Send a new `run_key` with the same
`chat_id` to continue the conversation. Retrying the same key and prompt returns
the same IDs. A second request can start after the previous run has ended.

The first request can set an IANA `time_zone` such as `Asia/Kolkata`. It defaults
to UTC. The root keeps its workspace and journal across requests. Wakefold
adds the current date in the chat's time zone to the end of each model
request. The saved journal holds the conversation and its append-only events.

The root loads the `agents` skill and dispatches `agents.spawn`
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

`GET /chats` lists up to 128 saved chats, newest root first, including empty
chats. Pass the last `root_agent_id` as `before` to read the next page.

`GET /chats/{chat_id}/runs` lists the root's requests and child runs.
`GET /chats/{chat_id}/history?after=<sequence>` reads the root's journal across
requests. `GET /runs/{run_id}/events` streams events for one request over
WebSocket. History and run events use the agent's journal sequence numbers.

`GET /runs/{run_id}/messages` returns up to 128 incoming messages. Pass the last
`arrival_order` as `after` for the next page.

## Artifacts

Agents can load the built-in `artifacts` skill. Set `artifacts.publish` to `require_approval` in the shared tool grants if publication needs approval. The tool publishes an existing file by title and chat-relative `path`. Pass its returned `artifact_id` with a later publication to add a revision.

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

Add a `sandbox_command` tool to a configured skill. Its approval rule goes
in `defaults.tool_grants`. The daemon sends the model's JSON arguments on
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
