-- Schema v11: per-skill usage records (spec: skills, Skill usage records).
-- One row per skill name of the profile, updated in the same transaction
-- as the counter it bumps; never transmitted off the machine. `state`
-- and `created_by` mirror `skill_usage::{SkillState, CreatedBy}`; times
-- are Unix milliseconds. A new table only: older binaries read the rest
-- unchanged.

CREATE TABLE skill_usage (
    name             TEXT    NOT NULL PRIMARY KEY,
    use_count        INTEGER NOT NULL DEFAULT 0 CHECK (use_count >= 0),
    view_count       INTEGER NOT NULL DEFAULT 0 CHECK (view_count >= 0),
    patch_count      INTEGER NOT NULL DEFAULT 0 CHECK (patch_count >= 0),
    last_activity_at INTEGER,
    state            TEXT    NOT NULL DEFAULT 'active'
                     CHECK (state IN ('active', 'stale', 'archived')),
    pinned           INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    created_by       TEXT    NOT NULL DEFAULT 'user'
                     CHECK (created_by IN ('user', 'agent', 'hub', 'bundled'))
) WITHOUT ROWID;
