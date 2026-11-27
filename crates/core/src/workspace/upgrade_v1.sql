-- At v1 each run also owned an agent identity and folder. Keep those IDs.
ALTER TABLE agent_runs RENAME TO old_runs;
ALTER TABLE run_events RENAME TO old_events;
ALTER TABLE run_waits RENAME TO old_waits;
ALTER TABLE mailbox_messages RENAME TO old_mailbox;
ALTER TABLE artifacts RENAME TO old_artifacts;
ALTER TABLE artifact_revisions RENAME TO old_revisions;
