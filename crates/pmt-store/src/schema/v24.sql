-- Schema v24 (spec: scheduled-jobs).
--
-- `cron_jobs`: one row per job. `spec` is the job's JSON definition
-- (schedule, payload, delivery, toolsets, memory, grace, timeout, repeat)
-- owned by pmt-cron; the columns beside it are what the scheduler queries
-- and updates field by field, so concurrent writers never clobber each
-- other's columns.
-- `cron_runs`: one row per occurrence (or manual run); the unique key on
-- (job_id, scheduled_at) is the at-most-once claim. Rows outlive their
-- job (run retention). `pid`/`process_start`/`host_id` name the process
-- executing it, so a later scheduler finalizes a run whose owner died.
-- `cron_deliveries`: the durable delivery outbox, one row per run and
-- target, sent at most once.
-- `cron_lease`: the single per-profile tick lease.

CREATE TABLE cron_jobs (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    spec             TEXT NOT NULL,
    origin           TEXT,
    state            TEXT NOT NULL CHECK (state IN ('scheduled', 'paused', 'running',
                         'completed', 'error')),
    enabled          INTEGER NOT NULL,
    next_run_at      INTEGER,
    last_run_at      INTEGER,
    last_status      TEXT CHECK (last_status IS NULL OR last_status IN ('ok', 'silent',
                         'failed', 'timed_out', 'blocked_config', 'delivery_queued',
                         'delivery_failed', 'skipped_late')),
    last_error       TEXT,
    completed        INTEGER NOT NULL DEFAULT 0,
    paused_at        INTEGER,
    paused_reason    TEXT,
    alerted          INTEGER NOT NULL DEFAULT 0,
    completed_at     INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
) STRICT;

CREATE INDEX cron_jobs_due ON cron_jobs (enabled, next_run_at);

CREATE TABLE cron_runs (
    id             TEXT PRIMARY KEY,
    job_id         TEXT NOT NULL,
    job_name       TEXT NOT NULL,
    scheduled_at   INTEGER NOT NULL,
    trigger        TEXT NOT NULL CHECK (trigger IN ('schedule', 'manual', 'replay')),
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
    UNIQUE (job_id, scheduled_at)
) STRICT;

CREATE INDEX cron_runs_job ON cron_runs (job_id, scheduled_at DESC);
CREATE INDEX cron_runs_status ON cron_runs (status);

CREATE TABLE cron_deliveries (
    id              TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL,
    job_id          TEXT NOT NULL,
    target          TEXT NOT NULL,
    body            TEXT NOT NULL,
    warning         INTEGER NOT NULL DEFAULT 0,
    state           TEXT NOT NULL CHECK (state IN ('pending', 'sending', 'sent', 'failed',
                        'suppressed', 'unknown')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    error           TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (run_id, target)
) STRICT;

CREATE INDEX cron_deliveries_state ON cron_deliveries (state, next_attempt_at);

CREATE TABLE cron_lease (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    token         TEXT NOT NULL,
    kind          TEXT NOT NULL,
    pid           INTEGER NOT NULL,
    process_start INTEGER NOT NULL,
    host_id       TEXT NOT NULL,
    acquired_at   INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL
) STRICT;
