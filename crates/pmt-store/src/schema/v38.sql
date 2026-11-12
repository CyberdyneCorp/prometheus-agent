-- Schema v38 (spec: api-server).
--
-- `api_sessions`: what the API server keeps beside an API session: the
-- caller instructions fixed when it was created, the caller's session key
-- (memory scope and conversation to continue) and the derived key of a
-- Chat Completions conversation (hash of its system prompt and first user
-- message). A deleted session takes its row with it, so a derived key
-- never maps to a deleted session's id again.
CREATE TABLE api_sessions (
    session_id   TEXT    PRIMARY KEY,
    instructions TEXT,
    session_key  TEXT,
    derived_key  TEXT    UNIQUE,
    created_at   INTEGER NOT NULL
) STRICT;
CREATE INDEX api_sessions_key ON api_sessions (session_key, created_at);

-- `api_responses`: the Responses API store; `used_at` orders the LRU
-- eviction past `api_server.responses.max_stored`.
CREATE TABLE api_responses (
    id           TEXT    PRIMARY KEY,
    session_id   TEXT    NOT NULL,
    conversation TEXT,
    body         TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    used_at      INTEGER NOT NULL
) STRICT;
CREATE INDEX api_responses_used ON api_responses (used_at);
CREATE INDEX api_responses_conversation ON api_responses (conversation, created_at);

-- `api_admissions`: idempotency keys of run-starting requests, reserved
-- before any work starts. `key_fp` is a fingerprint of the presenting
-- bearer key, `identity` the hash of the request's identity fields.
-- `answer` is the replayable answer (a completion, a response or the run
-- id); a deleted session's admissions keep their row as a tombstone until
-- `tombstone_until`, so the key cannot run new work meanwhile.
CREATE TABLE api_admissions (
    id              TEXT    PRIMARY KEY,
    endpoint        TEXT    NOT NULL CHECK (endpoint IN ('chat', 'responses', 'runs')),
    idem_key        TEXT    NOT NULL,
    key_fp          TEXT    NOT NULL,
    identity        TEXT    NOT NULL,
    session_id      TEXT,
    answer          TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    tombstone_until INTEGER,
    UNIQUE (endpoint, key_fp, idem_key)
) STRICT;
CREATE INDEX api_admissions_session ON api_admissions (session_id);

-- `api_runs`: the Runs API's run records. `owner` is the daemon instance
-- that executes the run: a non-terminal run of another instance is
-- `unknown` until resolved.
CREATE TABLE api_runs (
    id                   TEXT    PRIMARY KEY,
    session_id           TEXT    NOT NULL,
    status               TEXT    NOT NULL,
    admission_id         TEXT    NOT NULL,
    execution_generation INTEGER NOT NULL DEFAULT 1,
    owner                TEXT    NOT NULL,
    requested            TEXT,
    output               TEXT,
    usage                TEXT,
    runtime              TEXT,
    error                TEXT,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL
) STRICT;
CREATE INDEX api_runs_status ON api_runs (status, updated_at);
CREATE INDEX api_runs_session ON api_runs (session_id, created_at);
