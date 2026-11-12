-- Schema v36 (spec: hermes-migration, Sessions imported with lineage;
-- Idempotent incremental re-run).
--
-- `session_legacy_ids`: the id a session had in the agent it was imported
-- from (`source` = `hermes`, ...), so `--resume <old id>` resolves and a
-- re-run imports only what is new. `messages` is how many source messages
-- the import has carried so far.
CREATE TABLE session_legacy_ids (
    source     TEXT    NOT NULL,
    legacy_id  TEXT    NOT NULL,
    session_id TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    messages   INTEGER NOT NULL DEFAULT 0 CHECK (messages >= 0),
    PRIMARY KEY (source, legacy_id)
) STRICT;
CREATE INDEX session_legacy_ids_legacy ON session_legacy_ids (legacy_id);
CREATE INDEX session_legacy_ids_session ON session_legacy_ids (session_id);
