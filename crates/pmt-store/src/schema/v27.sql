-- Schema v27 (spec: session-automation, Persisted per-session automation
-- state): at most one goal, one loop and one heartbeat per session. `state`
-- is the JSON the automation engine owns (pmt-automation); `owner_surface`
-- is the surface that set it (`cli`, `tui`, `desktop`, `gateway`...), so a
-- heartbeat set from a chat keeps firing from the gateway into that chat.
-- Rows go with their session (delete, reset, a messaging conversation
-- switching away), never with a compaction or a restart.

CREATE TABLE automation (
    session_id    TEXT    NOT NULL REFERENCES sessions (id),
    kind          TEXT    NOT NULL CHECK (kind IN ('goal', 'loop', 'heartbeat')),
    state         TEXT    NOT NULL CHECK (json_valid(state)),
    owner_surface TEXT    NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (session_id, kind)
) WITHOUT ROWID;
