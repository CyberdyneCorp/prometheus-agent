-- Schema v15 (spec: messaging-gateway, Conversations never rotate on
-- time; Documented pairing flow; Durable delivery outbox; Crash recovery
-- and clean shutdown marker; Stuck-loop suspension). New tables only:
-- older binaries read the rest unchanged. Times are Unix ms.
--
-- `gateway_routes`: which session a conversation key routes to (the key
-- grammar is the gateway's; pruning a route never touches the session).
-- `gateway_sessions`: the gateway's state of a session it routes to: why
-- it replaced the previous one, whether it is suspended, and the restart
-- bookkeeping of crash recovery.
-- `gateway_pairing`: pending pairing requests (the code only as a salted
-- SHA-256) and approved senders, per platform.
-- `gateway_pairing_failures`: failed approvals, for the lockout.
-- `gateway_outbox`: every outbound message before its first send attempt
-- (`edit_of`: the platform message it replaces, for a streamed draft's
-- final text or a resolved approval card).

CREATE TABLE gateway_routes (
    key              TEXT    PRIMARY KEY,
    session_id       TEXT    NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    platform         TEXT    NOT NULL,
    chat_id          TEXT    NOT NULL,
    thread_id        TEXT,
    created_at       INTEGER NOT NULL,
    last_activity_at INTEGER NOT NULL
) STRICT;

CREATE INDEX gateway_routes_activity ON gateway_routes (last_activity_at);

CREATE TABLE gateway_sessions (
    session_id     TEXT    PRIMARY KEY REFERENCES sessions (id) ON DELETE CASCADE,
    route_key      TEXT    NOT NULL,
    reset_reason   TEXT    CHECK (reset_reason IN ('new', 'reset', 'suspended', 'stale_route',
                               'policy')),
    state          TEXT    NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'suspended')),
    turn_active    INTEGER NOT NULL DEFAULT 0,
    restart_count  INTEGER NOT NULL DEFAULT 0,
    resume_pending INTEGER NOT NULL DEFAULT 0,
    resume_reason  TEXT    CHECK (resume_reason IN ('restart_interrupted'))
) STRICT;

CREATE TABLE gateway_pairing (
    platform     TEXT    NOT NULL,
    sender_id    TEXT    NOT NULL,
    chat_id      TEXT    NOT NULL,
    sender_name  TEXT,
    state        TEXT    NOT NULL CHECK (state IN ('pending', 'approved')),
    code_salt    TEXT,
    code_hash    TEXT,
    requested_at INTEGER NOT NULL,
    expires_at   INTEGER,
    approved_at  INTEGER,
    PRIMARY KEY (platform, sender_id),
    CHECK ((state = 'pending') = (code_hash IS NOT NULL))
) STRICT;

CREATE TABLE gateway_pairing_failures (
    platform TEXT    NOT NULL,
    at       INTEGER NOT NULL
) STRICT;

CREATE INDEX gateway_pairing_failures_platform ON gateway_pairing_failures (platform, at);

CREATE TABLE gateway_outbox (
    id                  INTEGER PRIMARY KEY,
    idempotency_key     TEXT    NOT NULL UNIQUE,
    session_id          TEXT,
    platform            TEXT    NOT NULL,
    chat_id             TEXT    NOT NULL,
    thread_id           TEXT,
    reply_to            TEXT,
    edit_of             TEXT,
    kind                TEXT    NOT NULL CHECK (kind IN ('reply', 'notification', 'approval',
                                    'system')),
    body                TEXT    NOT NULL,
    state               TEXT    NOT NULL CHECK (state IN ('queued', 'sending', 'sent',
                                    'delivered', 'read', 'failed', 'unknown')),
    attempts            INTEGER NOT NULL DEFAULT 0,
    next_attempt_at     INTEGER NOT NULL,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    platform_message_id TEXT,
    last_error          TEXT
) STRICT;

CREATE INDEX gateway_outbox_due ON gateway_outbox (state, next_attempt_at);
