-- Schema v1: store identity, sessions with typed lineage, messages,
-- blob references, and turn leases / compaction locks.
-- Timestamps are Unix milliseconds. Enum columns carry CHECK constraints
-- that mirror the Rust enums in model.rs.

CREATE TABLE store_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE sessions (
    id                 TEXT PRIMARY KEY,
    profile_id         TEXT NOT NULL,
    source             TEXT NOT NULL CHECK (source IN ('cli', 'tui', 'gateway', 'cron',
                           'subagent', 'acp', 'api', 'desktop', 'dashboard', 'eval',
                           'harness', 'import')),
    origin             TEXT NOT NULL,
    model              TEXT,
    provider           TEXT,
    cwd                TEXT,
    git_repo_root      TEXT,
    git_branch         TEXT,
    title              TEXT,
    title_source       TEXT CHECK (title_source IN ('derived', 'llm', 'user')),
    started_at         INTEGER NOT NULL,
    last_activity_at   INTEGER NOT NULL,
    ended_at           INTEGER,
    end_reason         TEXT CHECK (end_reason IN ('user_exit', 'new_session', 'compression',
                           'budget_exhausted', 'error', 'interrupted', 'idle_timeout',
                           'job_complete')),
    pinned             INTEGER NOT NULL DEFAULT 0,
    archived           TEXT NOT NULL DEFAULT 'none' CHECK (archived IN ('none', 'user', 'auto')),
    hidden             INTEGER NOT NULL DEFAULT 0,
    bot_chat           INTEGER NOT NULL DEFAULT 0,
    system_prompt_hash TEXT,
    parent_session_id  TEXT,
    lineage_kind       TEXT CHECK (lineage_kind IN ('compression', 'fork', 'delegate',
                           'reset', 'import')),
    root_session_id    TEXT NOT NULL,
    message_count      INTEGER NOT NULL DEFAULT 0,
    tool_call_count    INTEGER NOT NULL DEFAULT 0,
    compaction_count   INTEGER NOT NULL DEFAULT 0,
    CHECK ((parent_session_id IS NULL) = (lineage_kind IS NULL)),
    CHECK ((title IS NULL) = (title_source IS NULL)),
    CHECK ((ended_at IS NULL) = (end_reason IS NULL))
) STRICT;

CREATE UNIQUE INDEX sessions_title ON sessions (title) WHERE title IS NOT NULL;
CREATE INDEX sessions_parent ON sessions (parent_session_id) WHERE parent_session_id IS NOT NULL;
CREATE INDEX sessions_root ON sessions (root_session_id);
CREATE INDEX sessions_activity ON sessions (last_activity_at);

-- Payload columns hold the value inline or, above 64 KiB, NULL plus a
-- `blob:sha256:<hex>` reference in the matching `*_ref` column.
CREATE TABLE messages (
    id                   TEXT PRIMARY KEY,
    session_id           TEXT NOT NULL REFERENCES sessions (id),
    seq                  INTEGER NOT NULL,
    role                 TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
    content              TEXT,
    content_ref          TEXT,
    tool_calls           TEXT,
    tool_calls_ref       TEXT,
    reasoning            TEXT,
    reasoning_ref        TEXT,
    provider_payload     TEXT,
    provider_payload_ref TEXT,
    tool_call_id         TEXT,
    tool_name            TEXT,
    metadata             TEXT,
    content_hash         TEXT NOT NULL,
    created_at           INTEGER NOT NULL,
    active               INTEGER NOT NULL DEFAULT 1,
    compacted            INTEGER NOT NULL DEFAULT 0,
    compaction_epoch     INTEGER,
    UNIQUE (session_id, seq)
) STRICT;

-- Every canonical row that references a blob; a blob with no row here is
-- garbage once older than the collection grace period.
CREATE TABLE blob_refs (
    sha256     TEXT NOT NULL,
    owner_kind TEXT NOT NULL,
    owner_id   TEXT NOT NULL,
    PRIMARY KEY (sha256, owner_kind, owner_id)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER messages_blob_refs_insert AFTER INSERT ON messages BEGIN
    INSERT OR IGNORE INTO blob_refs (sha256, owner_kind, owner_id)
    SELECT substr(ref, 13), 'message', NEW.id
    FROM (SELECT NEW.content_ref AS ref UNION ALL SELECT NEW.tool_calls_ref
          UNION ALL SELECT NEW.reasoning_ref UNION ALL SELECT NEW.provider_payload_ref)
    WHERE ref IS NOT NULL;
END;

CREATE TRIGGER messages_blob_refs_delete AFTER DELETE ON messages BEGIN
    DELETE FROM blob_refs WHERE owner_kind = 'message' AND owner_id = OLD.id;
END;

-- At most one turn lease and one compaction lock per session.
CREATE TABLE leases (
    session_id    TEXT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('turn', 'compaction')),
    token         TEXT NOT NULL,
    pid           INTEGER NOT NULL,
    process_start INTEGER NOT NULL,
    host_id       TEXT NOT NULL,
    acquired_at   INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    PRIMARY KEY (session_id, kind)
) STRICT;
