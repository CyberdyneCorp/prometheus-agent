-- Schema v30 (spec: agent-turn-loop, Verify-on-stop; Prefill messages).
--
-- The verify-on-stop evidence ledger, written only while
-- `agent.verify_on_stop` is on: `edit` rows name a non-documentation file a
-- turn changed, `check` rows a test, build, lint or type-check command it
-- ran. Rows go with their session.

CREATE TABLE verify_ledger (
    id         INTEGER PRIMARY KEY,
    session_id TEXT    NOT NULL REFERENCES sessions (id),
    turn_id    TEXT    NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('edit', 'check')),
    detail     TEXT    NOT NULL,
    at         INTEGER NOT NULL
) STRICT;

CREATE INDEX verify_ledger_session ON verify_ledger (session_id, id);

-- (spec: agent-turn-loop, Prefill messages) The few-shot messages of
-- `prompt.prefill_messages_file` a session froze at its start: the file's
-- path, its content hash and the messages (a JSON array of {role,
-- content}). They are sent after the system prompt and never stored as
-- transcript rows.
CREATE TABLE session_prefill (
    session_id TEXT PRIMARY KEY REFERENCES sessions (id),
    path       TEXT NOT NULL,
    sha256     TEXT NOT NULL,
    messages   TEXT NOT NULL CHECK (json_valid(messages))
) STRICT;
