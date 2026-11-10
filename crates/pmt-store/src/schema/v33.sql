-- Schema v33 (spec: session-storage, Prompt refresh and retitling).
--
-- Sessions marked by `prometheus sessions refresh-prompts`: the next turn
-- rebuilds the stored system prompt (reason `refresh-prompts`) and saving
-- it clears the mark. Rows go with their session.
CREATE TABLE prompt_refresh (
    session_id TEXT    NOT NULL PRIMARY KEY REFERENCES sessions (id),
    marked_at  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
