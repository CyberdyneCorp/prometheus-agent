-- Schema v32 (spec: memory, Memory revision journal; Reviewable memory
-- changes per session; Semantic recall).
--
-- `memory_revisions`: every committed memory change, written in the same
-- transaction as the change. `before` / `after` are the entry as JSON
-- (NULL for an add's before and a remove's after), so a revision can be
-- reverted exactly. `rev` only grows (AUTOINCREMENT never reuses one).
CREATE TABLE memory_revisions (
    rev        INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id   TEXT    NOT NULL,
    target     TEXT    NOT NULL CHECK (target IN ('memory', 'user')),
    operation  TEXT    NOT NULL CHECK (operation IN ('add', 'patch', 'remove', 'update')),
    before     TEXT,
    after      TEXT,
    origin     TEXT    NOT NULL
               CHECK (origin IN ('user', 'agent', 'background_review', 'provider', 'consolidation', 'import')),
    session_id TEXT,
    at         INTEGER NOT NULL,
    -- Set once the user kept or reverted it in `/memory review`.
    reviewed   INTEGER NOT NULL DEFAULT 0 CHECK (reviewed IN (0, 1))
);

CREATE INDEX memory_revisions_session ON memory_revisions (session_id, rev);
CREATE INDEX memory_revisions_entry ON memory_revisions (entry_id, rev);

-- Lexical recall ranks archival entries with FTS5 BM25. Maintained by the
-- memory write path (like messages_fts), keyed by the entry id.
CREATE VIRTUAL TABLE memory_fts USING fts5(
    content, entry_id UNINDEXED, tokenize = 'porter unicode61 remove_diacritics 2'
);

INSERT INTO memory_fts (content, entry_id) SELECT content, id FROM memory_entries;

-- Local embeddings of entries for semantic recall: one vector (little-endian
-- f32) per entry and embedding model, with the sha256 of the content it was
-- computed from (a changed content or model recomputes it).
CREATE TABLE memory_embeddings (
    entry_id    TEXT NOT NULL,
    model       TEXT NOT NULL,
    content_sha TEXT NOT NULL,
    vector      BLOB NOT NULL,
    PRIMARY KEY (entry_id, model)
) STRICT, WITHOUT ROWID;
