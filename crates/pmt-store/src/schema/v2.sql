-- Schema v2: rewind and deletion bookkeeping, the integration source tag,
-- derived full-text indexes and the usage ledger.

-- The sessions table is rebuilt (SQLite cannot change a CHECK in place):
-- a fork or reset child whose parent was deleted keeps its lineage_kind
-- with a NULL parent and parent_deleted = 1. The step runs with foreign
-- keys off and checks them before committing (migrate.rs).

CREATE TABLE sessions_v2 (
    id                 TEXT PRIMARY KEY,
    profile_id         TEXT NOT NULL,
    source             TEXT NOT NULL CHECK (source IN ('cli', 'tui', 'gateway', 'cron',
                           'subagent', 'acp', 'api', 'desktop', 'dashboard', 'eval',
                           'harness', 'import')),
    source_tag         TEXT CHECK (source_tag GLOB '[a-z]*' AND length(source_tag) <= 32
                           AND source_tag NOT GLOB '*[^a-z0-9_-]*'),
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
    parent_deleted     INTEGER NOT NULL DEFAULT 0,
    root_session_id    TEXT NOT NULL,
    message_count      INTEGER NOT NULL DEFAULT 0,
    tool_call_count    INTEGER NOT NULL DEFAULT 0,
    compaction_count   INTEGER NOT NULL DEFAULT 0,
    rewind_count       INTEGER NOT NULL DEFAULT 0,
    CHECK (parent_session_id IS NULL OR lineage_kind IS NOT NULL),
    CHECK (lineage_kind IS NULL OR parent_session_id IS NOT NULL OR parent_deleted = 1),
    CHECK ((title IS NULL) = (title_source IS NULL)),
    CHECK ((ended_at IS NULL) = (end_reason IS NULL))
) STRICT;

INSERT INTO sessions_v2 (id, profile_id, source, origin, model, provider, cwd, git_repo_root,
    git_branch, title, title_source, started_at, last_activity_at, ended_at, end_reason, pinned,
    archived, hidden, bot_chat, system_prompt_hash, parent_session_id, lineage_kind,
    root_session_id, message_count, tool_call_count, compaction_count)
SELECT id, profile_id, source, origin, model, provider, cwd, git_repo_root, git_branch, title,
    title_source, started_at, last_activity_at, ended_at, end_reason, pinned, archived, hidden,
    bot_chat, system_prompt_hash, parent_session_id, lineage_kind, root_session_id,
    message_count, tool_call_count, compaction_count
FROM sessions;

DROP TABLE sessions;
ALTER TABLE sessions_v2 RENAME TO sessions;

CREATE UNIQUE INDEX sessions_title ON sessions (title) WHERE title IS NOT NULL;
CREATE INDEX sessions_parent ON sessions (parent_session_id) WHERE parent_session_id IS NOT NULL;
CREATE INDEX sessions_root ON sessions (root_session_id);
CREATE INDEX sessions_activity ON sessions (last_activity_at);
CREATE INDEX sessions_cwd ON sessions (cwd, last_activity_at);

-- The rewind that deactivated a row (the session's rewind_count after it),
-- so one rewind can be restored as a unit.
ALTER TABLE messages ADD COLUMN rewind_batch INTEGER;

-- Derived full-text indexes over message text, keyed by the messages
-- rowid: words (unicode61) and trigrams (CJK and short terms). They are
-- maintained by the store's write path, never by triggers, so a broken
-- index cannot fail a canonical write (store_meta.index_stale).
CREATE VIRTUAL TABLE messages_fts USING fts5(text, tokenize = 'unicode61 remove_diacritics 2');
CREATE VIRTUAL TABLE messages_trigram USING fts5(text, tokenize = 'trigram');

INSERT INTO messages_fts (rowid, text)
SELECT rowid, content FROM messages WHERE content IS NOT NULL;
INSERT INTO messages_trigram (rowid, text)
SELECT rowid, content FROM messages WHERE content IS NOT NULL;

-- Spilled (blob) contents cannot be read from SQL: such a store serves
-- searches from the canonical scan until `sessions repair --indexes`.
INSERT INTO store_meta (key, value)
SELECT 'index_stale', CASE WHEN EXISTS (SELECT 1 FROM messages WHERE content_ref IS NOT NULL)
    THEN '1' ELSE '0' END;

-- One row per model or metered tool call (observability, Per-call usage
-- ledger), keyed by the lineage root. Append-only: rows go only with
-- their session.
CREATE TABLE usage_ledger (
    call_id            TEXT PRIMARY KEY,
    ts                 INTEGER NOT NULL,
    profile            TEXT NOT NULL,
    session_id         TEXT NOT NULL REFERENCES sessions (id),
    root_session_id    TEXT NOT NULL,
    turn_id            TEXT,
    parent_call_id     TEXT,
    purpose            TEXT NOT NULL,
    purpose_detail     TEXT,
    provider           TEXT NOT NULL,
    model              TEXT NOT NULL,
    service_tier       TEXT,
    input_tokens       INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens      INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0),
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
    cost_usd           REAL,
    cost_status        TEXT NOT NULL CHECK (cost_status IN ('exact', 'estimated',
                           'subscription', 'unknown')),
    billing_source     TEXT NOT NULL CHECK (billing_source = 'local'
                           OR billing_source GLOB 'subscription:?*'
                           OR billing_source GLOB 'api_key:?*'
                           OR billing_source GLOB 'keyless:?*'),
    request_hash       TEXT,
    cache_hit          INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX usage_ledger_root ON usage_ledger (root_session_id);
CREATE INDEX usage_ledger_session ON usage_ledger (session_id);

CREATE TRIGGER usage_ledger_append_only BEFORE UPDATE ON usage_ledger BEGIN
    SELECT RAISE(ABORT, 'usage_ledger is append-only');
END;
