-- Schema v5: per-credential request counts and benches (spec:
-- model-providers, Credential pool selection strategies; Exhausted
-- credential cooldowns). One row per provider credential, keyed by the
-- credential's stable id (`secret:<NAME>`, `subscription:<label>`,
-- `borrowed:<cli>`); never a key or token. Shared by every process of the
-- profile, so a bench set by one session is honoured by all of them and
-- survives restarts. A new table only: stores of v2-v4 binaries read the
-- rest unchanged.

CREATE TABLE credential_state (
    provider      TEXT    NOT NULL,
    credential    TEXT    NOT NULL,
    request_count INTEGER NOT NULL DEFAULT 0 CHECK (request_count >= 0),
    last_used_at  INTEGER,
    benched_until INTEGER,
    bench_reason  TEXT,
    PRIMARY KEY (provider, credential)
) WITHOUT ROWID;
