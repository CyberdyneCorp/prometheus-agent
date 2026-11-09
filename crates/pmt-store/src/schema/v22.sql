-- Schema v22 (spec: subagent-delegation, Durable background completions).
--
-- `delegations`: one row per `delegate_task` spawn call; `delivery` tracks
-- the completion of a background call (`pending` until every child ended
-- and it reached the owning session once: `delivered`, or `dropped` after
-- 8 attempts or 48 hours). Synchronous calls are `inline` (nothing to
-- deliver later).
-- `subagents`: one row per child; `result` is its `SubagentResult` JSON
-- once terminal. A restart marks children still `queued` or `running` as
-- `interrupted`, so their completion is delivered all the same.

CREATE TABLE delegations (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    root_session_id TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    delivery        TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    completed_at    INTEGER,
    delivered_at    INTEGER
) STRICT;

CREATE INDEX delegations_delivery ON delegations (delivery, root_session_id);

CREATE TABLE subagents (
    id                TEXT PRIMARY KEY,
    delegation_id     TEXT NOT NULL REFERENCES delegations (id) ON DELETE CASCADE,
    task_index        INTEGER NOT NULL,
    session_id        TEXT,
    parent_session_id TEXT NOT NULL,
    root_session_id   TEXT NOT NULL,
    depth             INTEGER NOT NULL,
    goal              TEXT NOT NULL,
    status            TEXT NOT NULL,
    result            TEXT,
    created_at        INTEGER NOT NULL,
    finished_at       INTEGER
) STRICT;

CREATE INDEX subagents_delegation ON subagents (delegation_id, task_index);
CREATE INDEX subagents_status ON subagents (status);
