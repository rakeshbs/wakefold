CREATE TABLE chats (
    chat_id   BLOB PRIMARY KEY CHECK (length(chat_id) = 16),
    time_zone TEXT  -- The first root run sets this, using UTC if omitted.
) STRICT;

CREATE TABLE agent_runs (
    agent_id        INTEGER PRIMARY KEY,
    chat_id         BLOB NOT NULL REFERENCES chats(chat_id),
    parent_agent_id INTEGER,  -- NULL for a root.
    run_key         TEXT NOT NULL,
    -- Daemon runs save settings at admission. Embedded SDK callers supply their own provider.
    settings_json   TEXT CHECK (json_valid(settings_json) OR settings_json IS NULL),
    status          TEXT NOT NULL DEFAULT 'reserved'
                    CHECK (status IN ('reserved', 'runnable', 'waiting', 'completed', 'failed')),

    UNIQUE (chat_id, run_key),
    UNIQUE (chat_id, agent_id),  -- Target for same-chat foreign keys.
    FOREIGN KEY (chat_id, parent_agent_id) REFERENCES agent_runs(chat_id, agent_id),
    CHECK (parent_agent_id IS NULL OR (parent_agent_id <> agent_id AND settings_json IS NOT NULL))
) STRICT;

CREATE INDEX child_runs ON agent_runs(chat_id, parent_agent_id);
CREATE INDEX runnable_runs ON agent_runs(agent_id) WHERE status = 'runnable';

CREATE TABLE run_events (
    agent_id   INTEGER NOT NULL REFERENCES agent_runs(agent_id),
    sequence   INTEGER NOT NULL CHECK (sequence > 0),
    event_json TEXT NOT NULL CHECK (json_valid(event_json)),

    PRIMARY KEY (agent_id, sequence)
) STRICT, WITHOUT ROWID;

CREATE INDEX run_compactions ON run_events(agent_id, sequence)
    WHERE json_extract(event_json, '$.kind') = 'compacted';

-- Only outstanding waits have a row. The journal keeps their history.
CREATE TABLE run_waits (
    agent_id             INTEGER PRIMARY KEY REFERENCES agent_runs(agent_id),
    wait_id              TEXT NOT NULL UNIQUE,
    wait_kind            TEXT NOT NULL CHECK (wait_kind IN (
                             'timer', 'message', 'mailbox_message', 'webhook',
                             'approval', 'tool_approval'
                         )),
    wake_at_ms           INTEGER,  -- Unix time in milliseconds, for timers only.
    start_event_sequence INTEGER NOT NULL,

    CHECK ((wait_kind = 'timer') = (wake_at_ms IS NOT NULL)),
    FOREIGN KEY (agent_id, start_event_sequence) REFERENCES run_events(agent_id, sequence)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE INDEX pending_timers ON run_waits(wake_at_ms) WHERE wake_at_ms IS NOT NULL;
CREATE INDEX waits_by_kind ON run_waits(wait_kind, agent_id);

CREATE TABLE mailbox_messages (
    arrival_order           INTEGER PRIMARY KEY,
    message_id              TEXT NOT NULL UNIQUE,
    chat_id                 BLOB NOT NULL,
    from_agent_id           INTEGER,  -- NULL for a host message.
    to_agent_id             INTEGER NOT NULL,
    message_kind            TEXT NOT NULL CHECK (message_kind IN ('message', 'child_outcome')),
    payload_json            TEXT NOT NULL CHECK (json_valid(payload_json)),
    received_event_sequence INTEGER,  -- NULL until the recipient consumes the message.
    client_message_key      TEXT,     -- Host retry key, scoped to the recipient.

    FOREIGN KEY (chat_id, from_agent_id) REFERENCES agent_runs(chat_id, agent_id),
    FOREIGN KEY (chat_id, to_agent_id) REFERENCES agent_runs(chat_id, agent_id),
    FOREIGN KEY (to_agent_id, received_event_sequence) REFERENCES run_events(agent_id, sequence)
        DEFERRABLE INITIALLY DEFERRED,
    UNIQUE (to_agent_id, client_message_key),
    CHECK ((from_agent_id IS NULL) = (client_message_key IS NOT NULL)),
    CHECK (message_kind <> 'child_outcome' OR from_agent_id IS NOT NULL)
) STRICT;

CREATE INDEX unread_messages ON mailbox_messages(to_agent_id, arrival_order)
    WHERE received_event_sequence IS NULL;
CREATE UNIQUE INDEX one_outcome_per_child ON mailbox_messages(from_agent_id)
    WHERE message_kind = 'child_outcome';

CREATE TABLE artifacts (
    artifact_id INTEGER PRIMARY KEY,
    chat_id     BLOB NOT NULL REFERENCES chats(chat_id),

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

    PRIMARY KEY (artifact_id, revision),
    FOREIGN KEY (chat_id, artifact_id) REFERENCES artifacts(chat_id, artifact_id),
    FOREIGN KEY (chat_id, published_by_agent_id) REFERENCES agent_runs(chat_id, agent_id),
    UNIQUE (published_by_agent_id, tool_call_key)
) STRICT, WITHOUT ROWID;
