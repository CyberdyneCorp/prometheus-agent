-- Schema v21 (spec: execution-environments, Project run-recipe
-- verification).
--
-- `verification_runs`: outcomes of `prometheus verify` run by a session,
-- the verification evidence recorded for it.

CREATE TABLE verification_runs (
    id         INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    at         INTEGER NOT NULL,
    passed     INTEGER NOT NULL,
    summary    TEXT NOT NULL
) STRICT;

CREATE INDEX verification_runs_session ON verification_runs (session_id, at);
