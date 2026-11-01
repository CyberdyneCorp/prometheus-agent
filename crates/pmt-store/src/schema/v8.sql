-- Schema v8: the `todo` list of each session (spec: agent-turn-loop, todo
-- tool contract). The list is session state the turn loop re-injects after
-- compaction, so it must survive a restart with the session. One row per
-- session, replaced on every write; `items` is the JSON array of
-- {id, content, status, parent?} in priority order and `revision` the
-- monotonic write counter. A new table only: v2-v7 binaries read the rest
-- unchanged.

CREATE TABLE session_todos (
    session_id TEXT    NOT NULL PRIMARY KEY REFERENCES sessions (id),
    revision   INTEGER NOT NULL CHECK (revision >= 0),
    items      TEXT    NOT NULL CHECK (json_valid(items))
) WITHOUT ROWID;
