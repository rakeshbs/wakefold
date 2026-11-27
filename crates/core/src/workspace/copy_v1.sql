-- Continue the newest active root, or the newest finished root if none is active.
-- Earlier independent roots retain their journals and original workspaces.
INSERT INTO agent_sessions(
    agent_id, chat_id, is_chat_root, parent_agent_id, spawned_by_run_id, time_zone, settings_json
)
SELECT r.agent_id, r.chat_id, (r.agent_id IS preferred.agent_id),
       r.parent_agent_id, r.parent_agent_id,
       CASE WHEN r.agent_id IS preferred.agent_id THEN c.time_zone END, r.settings_json
FROM old_runs r JOIN chats c USING (chat_id)
LEFT JOIN old_runs preferred ON preferred.agent_id = (
    SELECT candidate.agent_id FROM old_runs candidate
    WHERE candidate.chat_id = r.chat_id AND candidate.parent_agent_id IS NULL
      AND candidate.settings_json IS NOT NULL
    ORDER BY (candidate.status IN ('runnable','waiting')) DESC, candidate.agent_id DESC LIMIT 1
);

INSERT INTO agent_sessions(chat_id, is_chat_root, time_zone)
SELECT c.chat_id, 1, c.time_zone FROM chats c
WHERE NOT EXISTS (
    SELECT 1 FROM agent_sessions s WHERE s.chat_id=c.chat_id AND s.is_chat_root=1
);

INSERT INTO agent_runs(run_id, agent_id, chat_id, run_key, status, wait_id, wait_kind, wake_at_ms, wait_event_sequence)
SELECT r.agent_id, r.agent_id, r.chat_id, r.run_key, r.status, w.wait_id, w.wait_kind, w.wake_at_ms, w.start_event_sequence
FROM old_runs r LEFT JOIN old_waits w USING(agent_id);

INSERT INTO run_events(agent_id, sequence, run_id, event_json)
SELECT agent_id, sequence, agent_id, event_json FROM old_events;

INSERT INTO mailbox_messages(
    arrival_order, message_id, chat_id, from_agent_id, to_agent_id, message_kind,
    payload_json, to_run_id, received_event_sequence, client_message_key
)
SELECT arrival_order, message_id, chat_id, from_agent_id, to_agent_id, message_kind,
       payload_json, to_agent_id, received_event_sequence, client_message_key
FROM old_mailbox;

INSERT INTO artifacts SELECT * FROM old_artifacts;
INSERT INTO artifact_revisions SELECT *, published_by_agent_id FROM old_revisions;

DROP TABLE old_revisions;
DROP TABLE old_artifacts;
DROP TABLE old_mailbox;
DROP TABLE old_waits;
DROP TABLE old_events;
DROP TABLE old_runs;
DROP TABLE chats;
