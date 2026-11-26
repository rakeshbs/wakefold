# Wakefold

Wakefold is a Rust library and local daemon for AI agents that can pause and resume across restarts. It records each run in SQLite so you can replay what happened.

Agents can wait for a timer, a message, or human approval. They can delegate work to other agents with their own tool permissions and token budgets, load skills when needed, and call tools from local Model Context Protocol (MCP) servers.
