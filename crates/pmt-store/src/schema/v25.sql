-- Schema v25 (spec: scheduled-jobs, M2 upgrades).
--
-- `cron_jobs` gains the run-time state the upgrades keep per job: the
-- failure streak, the automatic re-run ladder (`retry_at`, `retry_of`,
-- `retry_step`), a closed provider usage window (`quota_hold_until`), the
-- last unverified delivery target and the monitor's previous output.
-- `cron_runs` is rebuilt (SQLite cannot change a CHECK in place) to accept
-- the `retry` and `webhook` triggers and to record the run's re-run
-- origin, its transient run context, its Bot Chat receipt and an
-- unverified delivery.
-- `cron_deliveries` records the platform message id of a sent row (a sent
-- row without one is unverified) and whether the delivery opens a
-- conversation a reply continues.
-- `cron_incidents`: failures by (job, normalized error signature), alerted
-- once. `cron_notepad`: the per-job key-value notepad.
-- `cron_suggestions`: the suggested automations inbox.
-- `cron_receipts`: Bot Chat deliveries this profile admitted as turns,
-- one per receipt id, so a retried delivery never admits a second turn.

ALTER TABLE cron_jobs ADD COLUMN failure_streak INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cron_jobs ADD COLUMN retry_at INTEGER;
ALTER TABLE cron_jobs ADD COLUMN retry_of TEXT;
ALTER TABLE cron_jobs ADD COLUMN retry_step INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cron_jobs ADD COLUMN quota_hold_until INTEGER;
ALTER TABLE cron_jobs ADD COLUMN delivery_unverified TEXT;
ALTER TABLE cron_jobs ADD COLUMN monitor_hash TEXT;
ALTER TABLE cron_jobs ADD COLUMN monitor_output TEXT;

CREATE TABLE cron_runs_v25 (
    id             TEXT PRIMARY KEY,
    job_id         TEXT NOT NULL,
    job_name       TEXT NOT NULL,
    scheduled_at   INTEGER NOT NULL,
    trigger        TEXT NOT NULL CHECK (trigger IN ('schedule', 'manual', 'replay', 'retry',
                       'webhook')),
    lateness       TEXT CHECK (lateness IS NULL OR lateness IN ('on_time', 'late', 'missed')),
    collapsed      INTEGER NOT NULL DEFAULT 0,
    skipped        INTEGER NOT NULL DEFAULT 0,
    status         TEXT NOT NULL CHECK (status IN ('queued', 'running', 'ok', 'silent',
                       'failed', 'timed_out', 'blocked_config', 'delivery_queued',
                       'delivery_failed', 'skipped_late')),
    reason         TEXT,
    error          TEXT,
    started_at     INTEGER,
    ended_at       INTEGER,
    session_id     TEXT,
    output         TEXT,
    output_blob    TEXT,
    inputs         TEXT,
    delivered      TEXT,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    cost_usd       REAL NOT NULL DEFAULT 0,
    pid            INTEGER,
    process_start  INTEGER,
    host_id        TEXT,
    created_at     INTEGER NOT NULL,
    retry_of       TEXT,
    run_context    TEXT,
    receipt        TEXT,
    unverified     TEXT,
    UNIQUE (job_id, scheduled_at)
) STRICT;

INSERT INTO cron_runs_v25 (id, job_id, job_name, scheduled_at, trigger, lateness, collapsed,
    skipped, status, reason, error, started_at, ended_at, session_id, output, output_blob,
    inputs, delivered, input_tokens, output_tokens, cost_usd, pid, process_start, host_id,
    created_at)
SELECT id, job_id, job_name, scheduled_at, trigger, lateness, collapsed, skipped, status,
    reason, error, started_at, ended_at, session_id, output, output_blob, inputs, delivered,
    input_tokens, output_tokens, cost_usd, pid, process_start, host_id, created_at
FROM cron_runs;

DROP TABLE cron_runs;
ALTER TABLE cron_runs_v25 RENAME TO cron_runs;
CREATE INDEX cron_runs_job ON cron_runs (job_id, scheduled_at DESC);
CREATE INDEX cron_runs_status ON cron_runs (status);

ALTER TABLE cron_deliveries ADD COLUMN message_id TEXT;
ALTER TABLE cron_deliveries ADD COLUMN continuable INTEGER NOT NULL DEFAULT 0;

CREATE TABLE cron_incidents (
    id             TEXT PRIMARY KEY,
    job_id         TEXT NOT NULL,
    job_name       TEXT NOT NULL,
    signature      TEXT NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('detected', 'alerted', 'resolved', 'closed')),
    error          TEXT NOT NULL,
    occurrences    INTEGER NOT NULL DEFAULT 1,
    reminded       INTEGER NOT NULL DEFAULT 0,
    first_seen_at  INTEGER NOT NULL,
    last_seen_at   INTEGER NOT NULL,
    alerted_at     INTEGER,
    updated_at     INTEGER NOT NULL,
    UNIQUE (job_id, signature)
) STRICT;

CREATE INDEX cron_incidents_activity ON cron_incidents (updated_at DESC);

CREATE TABLE cron_notepad (
    job_id      TEXT NOT NULL,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (job_id, key)
) STRICT;

CREATE TABLE cron_suggestions (
    key          TEXT PRIMARY KEY,
    source       TEXT NOT NULL CHECK (source IN ('catalog', 'blueprint', 'usage', 'integration')),
    title        TEXT NOT NULL,
    schedule     TEXT NOT NULL,
    prompt       TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('pending', 'accepted', 'dismissed')),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;

CREATE TABLE cron_receipts (
    receipt      TEXT PRIMARY KEY,
    from_profile TEXT NOT NULL,
    job_id       TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    created_at   INTEGER NOT NULL
) STRICT;
