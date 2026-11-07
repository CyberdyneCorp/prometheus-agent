-- Schema v23 (spec: subagent-delegation, User-launched background
-- sessions, Review command; session-storage, Session record and source
-- enum, Typed session lineage).
--
-- A `/bg` task's session is recorded with `source = 'background'` and the
-- launching session as its parent through the `background` lineage kind:
-- the link is lineage only, so such a child starts its own root (its
-- usage, spawn tree and owed completions are its own). SQLite cannot change
-- a CHECK in place, so the sessions table is rebuilt as in v2 (foreign keys
-- off, checked before commit; migrate.rs).

CREATE TABLE sessions_v23 (
    id                   TEXT PRIMARY KEY,
    profile_id           TEXT NOT NULL,
    source               TEXT NOT NULL CHECK (source IN ('cli', 'tui', 'gateway', 'cron',
                             'subagent', 'acp', 'api', 'desktop', 'dashboard', 'eval',
                             'harness', 'import', 'background')),
    source_tag           TEXT CHECK (source_tag GLOB '[a-z]*' AND length(source_tag) <= 32
                             AND source_tag NOT GLOB '*[^a-z0-9_-]*'),
    origin               TEXT NOT NULL,
    model                TEXT,
    provider             TEXT,
    cwd                  TEXT,
    git_repo_root        TEXT,
    git_branch           TEXT,
    title                TEXT,
    title_source         TEXT CHECK (title_source IN ('derived', 'llm', 'user')),
    started_at           INTEGER NOT NULL,
    last_activity_at     INTEGER NOT NULL,
    ended_at             INTEGER,
    end_reason           TEXT CHECK (end_reason IN ('user_exit', 'new_session', 'compression',
                             'budget_exhausted', 'error', 'interrupted', 'idle_timeout',
                             'job_complete')),
    pinned               INTEGER NOT NULL DEFAULT 0,
    archived             TEXT NOT NULL DEFAULT 'none' CHECK (archived IN ('none', 'user', 'auto')),
    hidden               INTEGER NOT NULL DEFAULT 0,
    bot_chat             INTEGER NOT NULL DEFAULT 0,
    system_prompt_hash   TEXT,
    parent_session_id    TEXT,
    lineage_kind         TEXT CHECK (lineage_kind IN ('compression', 'fork', 'delegate',
                             'reset', 'import', 'background')),
    parent_deleted       INTEGER NOT NULL DEFAULT 0,
    root_session_id      TEXT NOT NULL,
    message_count        INTEGER NOT NULL DEFAULT 0,
    tool_call_count      INTEGER NOT NULL DEFAULT 0,
    compaction_count     INTEGER NOT NULL DEFAULT 0,
    rewind_count         INTEGER NOT NULL DEFAULT 0,
    trajectory_purged_at INTEGER,
    CHECK (parent_session_id IS NULL OR lineage_kind IS NOT NULL),
    CHECK (lineage_kind IS NULL OR parent_session_id IS NOT NULL OR parent_deleted = 1),
    CHECK ((title IS NULL) = (title_source IS NULL)),
    CHECK ((ended_at IS NULL) = (end_reason IS NULL))
) STRICT;

INSERT INTO sessions_v23 (id, profile_id, source, source_tag, origin, model, provider, cwd,
    git_repo_root, git_branch, title, title_source, started_at, last_activity_at, ended_at,
    end_reason, pinned, archived, hidden, bot_chat, system_prompt_hash, parent_session_id,
    lineage_kind, parent_deleted, root_session_id, message_count, tool_call_count,
    compaction_count, rewind_count, trajectory_purged_at)
SELECT id, profile_id, source, source_tag, origin, model, provider, cwd, git_repo_root,
    git_branch, title, title_source, started_at, last_activity_at, ended_at, end_reason, pinned,
    archived, hidden, bot_chat, system_prompt_hash, parent_session_id, lineage_kind,
    parent_deleted, root_session_id, message_count, tool_call_count, compaction_count,
    rewind_count, trajectory_purged_at
FROM sessions;

DROP TABLE sessions;
ALTER TABLE sessions_v23 RENAME TO sessions;

CREATE UNIQUE INDEX sessions_title ON sessions (title) WHERE title IS NOT NULL;
CREATE INDEX sessions_parent ON sessions (parent_session_id) WHERE parent_session_id IS NOT NULL;
CREATE INDEX sessions_root ON sessions (root_session_id);
CREATE INDEX sessions_activity ON sessions (last_activity_at);
CREATE INDEX sessions_cwd ON sessions (cwd, last_activity_at);

-- What heads a delegation's completion (`/review`'s `Review: <focus>`), so
-- a completion delivered after a restart keeps its label.
ALTER TABLE delegations ADD COLUMN label TEXT;
