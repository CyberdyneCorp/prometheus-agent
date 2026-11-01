-- Schema v12: built-in memory (spec: memory, Built-in store of typed
-- entries; Write approval and pending changes). Enum columns mirror
-- `model::{MemoryTarget, MemoryKind, MemoryTier, MemoryStatus,
-- MemoryOrigin}`; times are Unix milliseconds. `origin_session` is the
-- session that wrote the entry (kept as text: the entry outlives it).
-- New tables only: older binaries read the rest unchanged.

CREATE TABLE memory_entries (
    id               TEXT    NOT NULL PRIMARY KEY
                     CHECK (length(id) = 14 AND id GLOB 'm_[a-z2-7]*'),
    target           TEXT    NOT NULL CHECK (target IN ('memory', 'user')),
    kind             TEXT    NOT NULL DEFAULT 'fact'
                     CHECK (kind IN ('fact', 'preference', 'instruction', 'project', 'person', 'event')),
    tier             TEXT    NOT NULL DEFAULT 'core' CHECK (tier IN ('core', 'archival')),
    content          TEXT    NOT NULL CHECK (length(content) > 0),
    pinned           INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    status           TEXT    NOT NULL DEFAULT 'active'
                     CHECK (status IN ('active', 'dormant', 'blocked')),
    expires_at       INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    last_recalled_at INTEGER,
    recall_count     INTEGER NOT NULL DEFAULT 0 CHECK (recall_count >= 0),
    origin           TEXT    NOT NULL
                     CHECK (origin IN ('user', 'agent', 'background_review', 'provider', 'consolidation', 'import')),
    origin_session   TEXT
) WITHOUT ROWID;

CREATE INDEX memory_entries_by_target ON memory_entries (target, tier, status);

-- A staged change: the batch (JSON operations with resolved entry ids) and
-- the content of every entry it touches as it was when staged (JSON object
-- id -> content), so approving a stale proposal is refused.
CREATE TABLE memory_pending (
    id          TEXT    NOT NULL PRIMARY KEY,
    target      TEXT    NOT NULL CHECK (target IN ('memory', 'user')),
    operations  TEXT    NOT NULL,
    base        TEXT    NOT NULL,
    origin      TEXT    NOT NULL
                CHECK (origin IN ('user', 'agent', 'background_review', 'provider', 'consolidation', 'import')),
    session_id  TEXT,
    created_at  INTEGER NOT NULL
) WITHOUT ROWID;
