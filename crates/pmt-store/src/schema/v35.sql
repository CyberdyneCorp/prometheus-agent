-- Schema v35 (spec: skills, Curator configuration and scheduling; Curator
-- transitions, scope and backups; Curator audit ledger, single-edit
-- rollback, purge and reports).
--
-- `skill_ledger`: one row per skill mutation (curator, agent or user),
-- shown as `L<id>`. `files` is a JSON array of `{path, before, after}`
-- where `before`/`after` are the SHA-256 of the file content (kept in the
-- blob store and referenced from `blob_refs` with owner kind
-- `skill_ledger`) or null when the file did not exist. `bytes` is the
-- entry's size for `skills.ledger_max_bytes` compaction.
CREATE TABLE skill_ledger (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    at            INTEGER NOT NULL,
    actor         TEXT    NOT NULL CHECK (actor IN ('curator', 'agent', 'user')),
    action        TEXT    NOT NULL CHECK (action IN ('create', 'edit', 'patch', 'delete',
                      'write_file', 'remove_file', 'archive', 'restore', 'purge', 'rollback')),
    skill         TEXT    NOT NULL,
    absorbed_into TEXT,
    session_id    TEXT,
    files         TEXT    NOT NULL,
    bytes         INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0)
);
CREATE INDEX skill_ledger_skill ON skill_ledger (skill, id);

-- The curator's own state: one row per profile database.
CREATE TABLE curator_state (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    last_run_at INTEGER,
    paused      INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1))
);

-- `curator adopt <name>` puts a user skill under curator management.
ALTER TABLE skill_usage ADD COLUMN adopted INTEGER NOT NULL DEFAULT 0 CHECK (adopted IN (0, 1));
