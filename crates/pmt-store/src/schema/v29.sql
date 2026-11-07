-- Schema v29 (spec: agent-turn-loop, Deterministic turn journal; Crash
-- resume mid-tool-batch).
--
-- One row per turn event, committed before the next side effect starts
-- (a `tool_started` row before its tool runs), so the point a crash
-- interrupted is recoverable. `data` holds the canonical request hash
-- (`request_sent`), the stored result's blob ref (`tool_finished`) or the
-- exit reason (`turn_finished`). Only a session's newest turn is kept: a
-- new `turn_started` drops the rows of its earlier turns.

CREATE TABLE turn_journal (
    id         INTEGER PRIMARY KEY,
    session_id TEXT    NOT NULL REFERENCES sessions (id),
    turn_id    TEXT    NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('turn_started', 'request_sent',
                   'response_received', 'tool_started', 'tool_finished', 'turn_finished')),
    call_id    TEXT,
    tool       TEXT,
    effect     TEXT,
    idempotent INTEGER,
    data       TEXT,
    at         INTEGER NOT NULL
) STRICT;

CREATE INDEX turn_journal_session ON turn_journal (session_id, id);
