-- Schema v13: the terminal each interactive session was last used from
-- (spec: cli, Resume and continue resolution: `-c` prefers the session
-- last used from the same terminal). `terminal` is an opaque identity of
-- the controlling TTY device and session leader, never argv; times are
-- Unix milliseconds. A new table only: older binaries read the rest
-- unchanged.

CREATE TABLE session_terminals (
    session_id TEXT    NOT NULL PRIMARY KEY
               REFERENCES sessions (id) ON DELETE CASCADE,
    terminal   TEXT    NOT NULL CHECK (length(terminal) > 0),
    used_at    INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX session_terminals_by_terminal ON session_terminals (terminal, used_at);
