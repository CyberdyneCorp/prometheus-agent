-- Schema v9: the system prompt and tool schemas each session froze (spec:
-- agent-turn-loop, System prompt frozen per session). Later turns replay
-- these exact bytes and tool order so every request extends the cached
-- prefix; a rebuild (new model, compaction, `--now`) replaces the row and
-- records why. `tiers` is the JSON array of {tier, text} (static, profile,
-- session), `sha256` the hash of the joined prompt text, `tools` the JSON
-- array of tool schemas in request order. A new table only: v2-v8
-- binaries read the rest unchanged.

CREATE TABLE session_prompts (
    session_id TEXT    NOT NULL PRIMARY KEY REFERENCES sessions (id),
    tiers      TEXT    NOT NULL CHECK (json_valid(tiers)),
    sha256     TEXT    NOT NULL,
    tools      TEXT    NOT NULL CHECK (json_valid(tools)),
    provider   TEXT,
    model      TEXT,
    built_at   INTEGER NOT NULL,
    reason     TEXT    NOT NULL
) WITHOUT ROWID;
