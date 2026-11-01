-- Schema v7: a per-provider use sequence on credential_state (spec:
-- model-providers, Credential pool selection strategies). `round_robin`
-- takes the credential that served least recently; `last_used_at` has
-- millisecond precision, so two requests in the same millisecond tied and
-- the pool served the same key twice. `use_seq` is one more than the
-- provider's highest on every recorded use, so the order is total. NULL on
-- rows from v5/v6 until their next use (selection then falls back to
-- `last_used_at`). An added nullable column: v2-v6 binaries read the rest
-- unchanged.

ALTER TABLE credential_state ADD COLUMN use_seq INTEGER CHECK (use_seq > 0);
