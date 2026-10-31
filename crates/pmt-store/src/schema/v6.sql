-- Schema v6: the tool surface a session froze when it started (spec:
-- tool-system, Tool introspection commands; Tool surface frozen per
-- session), so `prometheus tools list --session <id>` reports what that
-- session was given rather than what the current config would give. One
-- row per session, replaced when the session rebuilds its surface
-- (compaction, `/tools reload --now`). `tools` is a JSON array of
-- {name, toolset, owner, exposure}. A new table only: stores of v2-v5
-- binaries read the rest unchanged.

CREATE TABLE session_tool_surface (
    session_id  TEXT    NOT NULL PRIMARY KEY REFERENCES sessions (id),
    generation  INTEGER NOT NULL CHECK (generation >= 0),
    recorded_at INTEGER NOT NULL,
    tools       TEXT    NOT NULL CHECK (json_valid(tools))
) WITHOUT ROWID;
