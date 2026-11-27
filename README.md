# Wakefold

Wakefold is a Rust library and local daemon for AI agents that can pause and resume across restarts. It records each run in SQLite so you can replay what happened.

Agents can wait for a timer, a message, or human approval. They can delegate work to other agents with their own tool permissions and token budgets, load skills when needed, and call tools from local Model Context Protocol (MCP) servers.

## Artifacts

Enable the built-in `artifacts` skill for an agent and grant `artifacts.publish` with `allow` or `require_approval`. The tool publishes an existing file by title and chat-relative `path`. Pass its returned `artifact_id` with a later publication to add a revision.

- `GET /artifacts?chat_key=…` lists each artifact's latest revision.
- `GET /artifacts/{artifact_id}/revisions/{revision}?chat_key=…` downloads that exact revision.

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

Set the daemon's top-level `sandbox` configuration. This example grants
agent ID 12 read-only access to agent ID 7's folder in the same chat:

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
writable, explicitly granted same-chat peer folders are read-only, and the
sandbox has no external network. Apple `container` provides the per-chat VM
and Bubblewrap applies the per-agent mounts inside it. Linux runs Bubblewrap
directly. Existing in-process and configured MCP tools run as trusted host
tools.

Run the real backend acceptance test with
`cargo test -p wakefold-daemon --features sandbox workspace_permissions_and_limits_survive_reuse -- --ignored`.
On macOS, set `WAKEFOLD_SANDBOX_TEST_IMAGE` to the guest image tag. Set
`WAKEFOLD_SANDBOX_TEST_CONTAINER` when using a custom CLI path.
