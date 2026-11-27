CREATE TABLE agent_sessions (
    agent_id          INTEGER PRIMARY KEY,
    chat_id           BLOB NOT NULL CHECK (length(chat_id) = 16),
    is_chat_root      INTEGER NOT NULL DEFAULT 0 CHECK (is_chat_root IN (0, 1)),
    parent_agent_id   INTEGER,
    spawned_by_run_id INTEGER,
    time_zone         TEXT,
    settings_json     TEXT CHECK (settings_json IS NULL OR json_valid(settings_json)),

    UNIQUE (chat_id, agent_id),
    FOREIGN KEY (chat_id, parent_agent_id) REFERENCES agent_sessions(chat_id, agent_id),
    FOREIGN KEY (parent_agent_id, spawned_by_run_id) REFERENCES agent_runs(agent_id, run_id),
    CHECK ((parent_agent_id IS NULL) = (spawned_by_run_id IS NULL)),
    CHECK (parent_agent_id IS NULL OR (parent_agent_id <> agent_id AND is_chat_root = 0)),
    CHECK (time_zone IS NULL OR is_chat_root = 1)
) STRICT;

CREATE UNIQUE INDEX conversation_roots ON agent_sessions(chat_id) WHERE is_chat_root = 1;
CREATE INDEX agent_children ON agent_sessions(parent_agent_id, agent_id);

CREATE TABLE agent_runs (
    run_id               INTEGER PRIMARY KEY,
    agent_id             INTEGER NOT NULL,
    chat_id              BLOB NOT NULL,
    run_key              TEXT NOT NULL,
    status               TEXT NOT NULL DEFAULT 'reserved'
                         CHECK (status IN ('reserved', 'runnable', 'waiting', 'completed', 'failed')),
    wait_id              TEXT UNIQUE,
    wait_kind            TEXT CHECK (wait_kind IN (
                             'timer', 'message', 'mailbox_message', 'webhook',
                             'approval', 'tool_approval'
                         )),
    wake_at_ms           INTEGER,
    wait_event_sequence  INTEGER,

    UNIQUE (chat_id, run_key),
    UNIQUE (agent_id, run_id),
    FOREIGN KEY (chat_id, agent_id) REFERENCES agent_sessions(chat_id, agent_id),
    FOREIGN KEY (run_id, wait_event_sequence) REFERENCES run_events(run_id, sequence)
        DEFERRABLE INITIALLY DEFERRED,
    CHECK ((status = 'waiting') = (wait_id IS NOT NULL)),
    CHECK ((wait_id IS NULL) = (wait_kind IS NULL)),
    CHECK ((wait_id IS NULL) = (wait_event_sequence IS NULL)),
    CHECK ((wait_kind IS 'timer') = (wake_at_ms IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX one_active_run ON agent_runs(agent_id) WHERE status IN ('runnable', 'waiting');
CREATE INDEX scheduled_runs ON agent_runs(run_id) WHERE status = 'runnable';
CREATE INDEX due_runs ON agent_runs(wake_at_ms, run_id) WHERE wake_at_ms IS NOT NULL;
CREATE INDEX waiting_runs ON agent_runs(wait_kind, run_id) WHERE wait_kind IS NOT NULL;

CREATE TABLE run_events (
    agent_id   INTEGER NOT NULL,
    sequence   INTEGER NOT NULL CHECK (sequence > 0),
    run_id     INTEGER NOT NULL,
    event_json TEXT NOT NULL CHECK (json_valid(event_json)),

    PRIMARY KEY (agent_id, sequence),
    UNIQUE (run_id, sequence),
    FOREIGN KEY (agent_id, run_id) REFERENCES agent_runs(agent_id, run_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX agent_compactions ON run_events(agent_id, sequence)
    WHERE json_extract(event_json, '$.kind') = 'compacted';
CREATE TRIGGER run_events_no_update BEFORE UPDATE ON run_events
    BEGIN SELECT RAISE(ABORT, 'run events are append-only'); END;
CREATE TRIGGER run_events_no_delete BEFORE DELETE ON run_events
    BEGIN SELECT RAISE(ABORT, 'run events are append-only'); END;

CREATE TABLE mailbox_messages (
    arrival_order           INTEGER PRIMARY KEY,
    message_id              TEXT NOT NULL UNIQUE,
    chat_id                 BLOB NOT NULL,
    from_agent_id           INTEGER,  -- NULL for a host message.
    to_agent_id             INTEGER NOT NULL,
    message_kind            TEXT NOT NULL CHECK (message_kind IN ('message', 'child_outcome')),
    payload_json            TEXT NOT NULL CHECK (json_valid(payload_json)),
    to_run_id               INTEGER,
    received_event_sequence INTEGER,  -- NULL until the recipient consumes the message.
    client_message_key      TEXT,     -- Host retry key, scoped to the recipient.

    FOREIGN KEY (chat_id, from_agent_id) REFERENCES agent_sessions(chat_id, agent_id),
    FOREIGN KEY (chat_id, to_agent_id) REFERENCES agent_sessions(chat_id, agent_id),
    FOREIGN KEY (to_agent_id, to_run_id) REFERENCES agent_runs(agent_id, run_id),
    FOREIGN KEY (to_agent_id, received_event_sequence) REFERENCES run_events(agent_id, sequence)
        DEFERRABLE INITIALLY DEFERRED,
    UNIQUE (to_run_id, client_message_key),
    CHECK ((from_agent_id IS NULL) = (client_message_key IS NOT NULL)),
    CHECK (client_message_key IS NULL OR to_run_id IS NOT NULL),
    CHECK (message_kind <> 'child_outcome' OR (from_agent_id IS NOT NULL AND to_run_id IS NOT NULL))
) STRICT;

CREATE INDEX mailbox_unread ON mailbox_messages(to_agent_id, arrival_order)
    WHERE received_event_sequence IS NULL;
CREATE UNIQUE INDEX mailbox_child_outcome ON mailbox_messages(from_agent_id)
    WHERE message_kind = 'child_outcome';

CREATE TABLE artifacts (
    artifact_id INTEGER PRIMARY KEY,
    chat_id     BLOB NOT NULL,

    UNIQUE (chat_id, artifact_id)
) STRICT;

CREATE TABLE artifact_revisions (
    artifact_id           INTEGER NOT NULL,
    revision              INTEGER NOT NULL CHECK (revision > 0),
    chat_id               BLOB NOT NULL,
    title                 TEXT NOT NULL,
    filename              TEXT NOT NULL,
    media_type            TEXT NOT NULL,
    size_bytes            INTEGER NOT NULL CHECK (size_bytes >= 0),
    published_by_agent_id INTEGER NOT NULL,
    tool_call_key         TEXT NOT NULL,
    published_in_run_id   INTEGER NOT NULL,

    PRIMARY KEY (artifact_id, revision),
    FOREIGN KEY (chat_id, artifact_id) REFERENCES artifacts(chat_id, artifact_id),
    FOREIGN KEY (chat_id, published_by_agent_id) REFERENCES agent_sessions(chat_id, agent_id),
    FOREIGN KEY (published_by_agent_id, published_in_run_id) REFERENCES agent_runs(agent_id, run_id),
    UNIQUE (published_by_agent_id, tool_call_key)
) STRICT, WITHOUT ROWID;
