-- Schema v45 (spec: bot-mode).
--
-- `bot_messages`: bot-to-bot deliveries (`message_agent`). The sending
-- profile keeps an `out` row (what it sent and how it ended); a target
-- served by the same home keeps an `in` row, the durable ingress record
-- its daemon runs as the target Bot Chat's next turn. Both share the
-- delivery id, so a delivery never runs twice. Times are Unix ms.
CREATE TABLE bot_messages (
    delivery_id     TEXT    NOT NULL,
    direction       TEXT    NOT NULL CHECK (direction IN ('in', 'out')),
    route           TEXT    NOT NULL CHECK (route IN ('local', 'peer', 'connection')),
    sender          TEXT    NOT NULL,
    attribution     TEXT    NOT NULL,
    target          TEXT    NOT NULL,
    message         TEXT    NOT NULL,
    hops            INTEGER NOT NULL,
    origin_session  TEXT,
    session_id      TEXT,
    state           TEXT    NOT NULL
                    CHECK (state IN ('queued', 'running', 'delivered', 'failed')),
    reason          TEXT,
    error           TEXT,
    reply           TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    notified        INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (delivery_id, direction)
) STRICT;

CREATE INDEX bot_messages_by_state ON bot_messages (direction, state, created_at);

-- Group chats (rooms). The home's rooms live in the default profile's
-- store: one log and one driver lease per room for every process sharing
-- the home. `role` is `authority` (this home owns the log and drives
-- turns), `replica` (a copy kept through `groups.replicate`) or `demoted`
-- (a former authority that adopted a newer lineage and refuses writes).
CREATE TABLE rooms (
    id                      TEXT    NOT NULL PRIMARY KEY,
    name                    TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    picture                 TEXT,
    detect_stop_directives  INTEGER NOT NULL DEFAULT 1,
    role                    TEXT    NOT NULL CHECK (role IN ('authority', 'replica', 'demoted')),
    authority_gateway       TEXT    NOT NULL,
    authority_epoch         INTEGER NOT NULL DEFAULT 1,
    source_peer             TEXT,
    latest_seq              INTEGER NOT NULL DEFAULT 0,
    needs_you               INTEGER NOT NULL DEFAULT 0,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
) STRICT;

-- A member: a local profile (`gateway` NULL) or an agent on a peer
-- gateway. `seen_seq` is the last log entry its turns saw; `held` stops
-- its turns until it is mentioned again or the room is addressed.
CREATE TABLE room_members (
    room_id     TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    member      TEXT    NOT NULL,
    profile     TEXT    NOT NULL,
    gateway     TEXT,
    held        INTEGER NOT NULL DEFAULT 0,
    seen_seq    INTEGER NOT NULL DEFAULT 0,
    state       TEXT    NOT NULL DEFAULT 'ok' CHECK (state IN ('ok', 'unreachable')),
    reason      TEXT,
    pending_run TEXT,
    position    INTEGER NOT NULL,
    PRIMARY KEY (room_id, member)
) STRICT;

-- The room log: messages and events in sequence order.
CREATE TABLE room_log (
    room_id  TEXT    NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    seq      INTEGER NOT NULL,
    kind     TEXT    NOT NULL,
    author   TEXT,
    text     TEXT,
    data     TEXT,
    at       INTEGER NOT NULL,
    PRIMARY KEY (room_id, seq)
) STRICT;

-- One driver per room.
CREATE TABLE room_leases (
    room_id     TEXT    NOT NULL PRIMARY KEY,
    holder      TEXT    NOT NULL,
    expires_at  INTEGER NOT NULL
) STRICT;

-- Room grants this home issued to other gateways' room authorities: a
-- hash of the token, never the token.
CREATE TABLE room_grants (
    token_hash  TEXT    NOT NULL PRIMARY KEY,
    room_id     TEXT    NOT NULL,
    authority   TEXT    NOT NULL,
    member      TEXT    NOT NULL,
    scopes      TEXT    NOT NULL,
    issued_at   INTEGER NOT NULL,
    revoked_at  INTEGER
) STRICT;

-- Authority runs of remote rooms this home's members took part in: a run
-- outlives the authority's connection and is reported at its next drive.
CREATE TABLE room_member_runs (
    run_id      TEXT    NOT NULL PRIMARY KEY,
    room_id     TEXT    NOT NULL,
    member      TEXT    NOT NULL,
    state       TEXT    NOT NULL CHECK (state IN ('running', 'done', 'failed')),
    reply       TEXT,
    reason      TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;
