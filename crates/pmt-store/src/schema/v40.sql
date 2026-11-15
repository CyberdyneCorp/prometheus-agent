-- Schema v40 (spec: agent-interop).
--
-- `a2a_bindings`: the exact identity of each inbound A2A conversation,
-- `<profile>:a2a:<peer>[@<tenant>]:<context>` (components percent-encoded),
-- bound to the session its turns run in. A context id is never matched by
-- session title: only this key finds the session again.
CREATE TABLE a2a_bindings (
    key        TEXT    PRIMARY KEY,
    session_id TEXT    NOT NULL,
    peer       TEXT    NOT NULL,
    tenant     TEXT,
    context_id TEXT    NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

-- `a2a_calls`: every outbound `a2a_*` exchange, by conversation
-- (`a2a_history`) and agent (`a2a_list` metrics).
CREATE TABLE a2a_calls (
    id          INTEGER PRIMARY KEY,
    context_id  TEXT    NOT NULL,
    agent       TEXT    NOT NULL,
    url         TEXT    NOT NULL,
    task_id     TEXT,
    message     TEXT    NOT NULL,
    reply       TEXT,
    error       TEXT,
    duration_ms INTEGER NOT NULL,
    at          INTEGER NOT NULL
) STRICT;
CREATE INDEX a2a_calls_context ON a2a_calls (context_id, id);

-- `bot_relay_outbox`: signed envelopes of bot messages bound for an agent
-- on another desktop connection, until a courier delivers them or they
-- time out.
CREATE TABLE bot_relay_outbox (
    delivery_id TEXT    PRIMARY KEY,
    sender      TEXT    NOT NULL,
    target      TEXT    NOT NULL,
    message     TEXT    NOT NULL,
    signature   TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    state       TEXT    NOT NULL,
    offers      INTEGER NOT NULL DEFAULT 0,
    first_offer INTEGER,
    offered_at  INTEGER,
    reply       TEXT,
    reason      TEXT,
    settled_at  INTEGER
) STRICT;

-- `bot_relay_inbox`: relayed deliveries this profile received, so a
-- delivery id never runs twice.
CREATE TABLE bot_relay_inbox (
    delivery_id TEXT    PRIMARY KEY,
    session_id  TEXT    NOT NULL,
    state       TEXT    NOT NULL,
    reply       TEXT,
    reason      TEXT,
    received_at INTEGER NOT NULL
) STRICT;

-- `peer_probes`: the last `GET /v1/capabilities` probe of each peer
-- gateway (`gateway peer list`).
CREATE TABLE peer_probes (
    name   TEXT    PRIMARY KEY,
    at     INTEGER NOT NULL,
    ok     INTEGER NOT NULL,
    detail TEXT    NOT NULL
) STRICT;
