-- Schema v28 (spec: model-providers, Provider health scoring; Subscription
-- plan and quota visibility; Provider spend caps).
--
-- `provider_calls`: one row per model call outcome per (provider, model,
-- credential id), the rolling window health scores are computed over;
-- `ok` is 1 on success, else `class` names the error class. Rows beyond
-- the newest 500 of a key are pruned on insert.
-- `subscription_quota`: the newest usage probe of one subscription
-- credential (`windows` is a JSON array of {name, used_percent, resets_at}),
-- shared by every process of the profile.
-- `provider_budget_warnings`: the spend-cap periods (`daily:YYYY-MM-DD`,
-- `monthly:YYYY-MM`) whose 80% warning a provider already emitted.

CREATE TABLE provider_calls (
    id         INTEGER PRIMARY KEY,
    provider   TEXT NOT NULL,
    model      TEXT NOT NULL,
    credential TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    ok         INTEGER NOT NULL,
    class      TEXT,
    ttft_ms    INTEGER,
    output_tps REAL
) STRICT;

CREATE INDEX provider_calls_key ON provider_calls (provider, model, credential, id);

CREATE TABLE subscription_quota (
    provider   TEXT NOT NULL,
    credential TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    plan       TEXT,
    windows    TEXT NOT NULL,
    PRIMARY KEY (provider, credential)
) STRICT;

CREATE TABLE provider_budget_warnings (
    provider  TEXT NOT NULL,
    period    TEXT NOT NULL,
    warned_at INTEGER NOT NULL,
    PRIMARY KEY (provider, period)
) STRICT;

-- `key_origins`: API keys that were minted rather than typed (an
-- OpenRouter key from `auth add openrouter --type oauth` has source
-- `oauth`); with no refresh token behind it, a 401 marks it `dead`.
CREATE TABLE key_origins (
    provider   TEXT NOT NULL,
    credential TEXT NOT NULL,
    source     TEXT NOT NULL,
    dead       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, credential)
) STRICT;
