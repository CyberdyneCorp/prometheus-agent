-- Schema v10: the automatic-compaction guards of each session (spec:
-- context-compression, Trigger decision and blocking reasons; Summary
-- failure handling). A restart must honour an armed summary-failure
-- cooldown, a backoff and the `ineffective` breaker, so they live with the
-- session instead of in process memory. Times are Unix milliseconds;
-- `pending_before` holds the context tokens before the last committed
-- compaction until the provider's next reported usage judges how much it
-- reclaimed. A new table only: v2-v9 binaries read the rest unchanged.

CREATE TABLE session_compression (
    session_id       TEXT    NOT NULL PRIMARY KEY REFERENCES sessions (id),
    summary_failures INTEGER NOT NULL DEFAULT 0 CHECK (summary_failures >= 0),
    cooldown_until   INTEGER,
    backoff_until    INTEGER,
    ineffective      INTEGER NOT NULL DEFAULT 0 CHECK (ineffective >= 0),
    pending_before   INTEGER CHECK (pending_before >= 0)
) WITHOUT ROWID;
