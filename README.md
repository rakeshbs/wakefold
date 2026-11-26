# Wakefold

Wakefold is a Rust library and local daemon for AI agents that can pause and resume across restarts. It records each run in SQLite so you can replay what happened.

Agents can wait for a timer, a message, or human approval. They can delegate work to other agents with their own tool permissions and token budgets, load skills when needed, and call tools from local Model Context Protocol (MCP) servers.

## Artifacts

Enable the built-in `artifacts` skill for an agent and grant `artifacts.publish` with `allow` or `require_approval`. The tool publishes an existing file by title and chat-relative `path`. Pass its returned `artifact_id` with a later publication to add a revision.

- `GET /artifacts?chat_key=…` lists each artifact's latest revision.
- `GET /artifacts/{artifact_id}/revisions/{revision}?chat_key=…` downloads that exact revision.

Published copies survive restarts and workspace edits. The run's event stream records the revision it delivered.
