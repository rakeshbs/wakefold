# Wakefold

Wakefold is a durable agent harness for Rust.
Its goal is to let agents resume after a crash and wait for events without keeping a task in memory.
The design uses an append-only journal to rebuild execution state and replay each step.
