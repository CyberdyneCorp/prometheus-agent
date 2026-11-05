-- Schema v20 (spec: execution-environments, Retained process receipts).
--
-- `process_receipts`: what a background process left behind once it
-- ended (command, exit code, times, the last 200,000 characters of its
-- redacted output). Readable only from the owning conversation, matched by
-- `root_session_id`; the newest 64 per profile are kept for at most 7 days.

CREATE TABLE process_receipts (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    root_session_id TEXT NOT NULL,
    command         TEXT NOT NULL,
    exit_code       INTEGER NOT NULL,
    started_at      INTEGER NOT NULL,
    ended_at        INTEGER NOT NULL,
    output          TEXT NOT NULL
) STRICT;

CREATE INDEX process_receipts_root ON process_receipts (root_session_id, ended_at);
