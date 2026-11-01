-- Schema v4: model context windows discovered from provider listings and
-- local server probes, reused across sessions (spec: model-providers,
-- Context length detection chain). Keyed by provider, model and base URL
-- so two endpoints serving the same model id keep separate windows. A new
-- table only: stores of v2/v3 binaries read the rest unchanged.

CREATE TABLE model_context (
    provider       TEXT    NOT NULL,
    model          TEXT    NOT NULL,
    base_url       TEXT    NOT NULL,
    context_window INTEGER NOT NULL CHECK (context_window > 0),
    source         TEXT    NOT NULL CHECK (source IN ('listing', 'probe')),
    discovered_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, model, base_url)
) WITHOUT ROWID;
