-- Schema v17 (spec: messaging-gateway, Discarding a restart-interrupted
-- turn; Adapter operations from chat; Runtime metadata footer; Warning
-- notification suppression; Intentional silence tokens; Interactive model
-- picker and chat model override).
--
-- `gateway_sessions.resume_at`: when a restart left the session's turn
-- `resume_pending` (the pending turn is discarded after
-- `gateway.sessions.resume_pending_ttl_secs`).
-- `gateway_sessions.timestamps`: `gateway.message_timestamps.enabled` as it
-- was when the session was created (frozen with it).
-- `gateway_adapter_pauses`: adapters an admin paused (`/platform pause`,
-- `prometheus gateway pause`); kept across restarts.
-- `gateway_chat_prefs`: per-chat choices made in the chat (`/footer`, the
-- `/model` override for new sessions).
-- `gateway_outbox` is rebuilt to accept kind `warning` (producer-classified
-- automatic warnings, mutable per destination) and the settled states
-- `suppressed` (a muted warning) and `silent` (an intentionally silent
-- reply). Builds older than v17 report such rows as corrupt in outbox
-- listings only; they still send queued entries.

ALTER TABLE gateway_sessions ADD COLUMN resume_at INTEGER;

ALTER TABLE gateway_sessions ADD COLUMN timestamps INTEGER NOT NULL DEFAULT 0;

CREATE TABLE gateway_adapter_pauses (
    platform  TEXT    PRIMARY KEY,
    paused_at INTEGER NOT NULL,
    actor     TEXT    NOT NULL
) STRICT;

CREATE TABLE gateway_chat_prefs (
    platform   TEXT    NOT NULL,
    chat_id    TEXT    NOT NULL,
    footer     INTEGER,
    model      TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (platform, chat_id)
) STRICT;

CREATE TABLE gateway_outbox_v17 (
    id                  INTEGER PRIMARY KEY,
    idempotency_key     TEXT    NOT NULL UNIQUE,
    session_id          TEXT,
    platform            TEXT    NOT NULL,
    chat_id             TEXT    NOT NULL,
    thread_id           TEXT,
    reply_to            TEXT,
    edit_of             TEXT,
    kind                TEXT    NOT NULL CHECK (kind IN ('reply', 'notification', 'approval',
                                    'system', 'warning')),
    body                TEXT    NOT NULL,
    state               TEXT    NOT NULL CHECK (state IN ('queued', 'sending', 'sent',
                                    'delivered', 'read', 'failed', 'unknown', 'suppressed',
                                    'silent')),
    attempts            INTEGER NOT NULL DEFAULT 0,
    next_attempt_at     INTEGER NOT NULL,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    platform_message_id TEXT,
    last_error          TEXT,
    attachments         TEXT
) STRICT;

INSERT INTO gateway_outbox_v17
    (id, idempotency_key, session_id, platform, chat_id, thread_id, reply_to, edit_of, kind,
     body, state, attempts, next_attempt_at, created_at, updated_at, platform_message_id,
     last_error, attachments)
SELECT id, idempotency_key, session_id, platform, chat_id, thread_id, reply_to, edit_of, kind,
       body, state, attempts, next_attempt_at, created_at, updated_at, platform_message_id,
       last_error, attachments
FROM gateway_outbox;

DROP TABLE gateway_outbox;

ALTER TABLE gateway_outbox_v17 RENAME TO gateway_outbox;

CREATE INDEX gateway_outbox_due ON gateway_outbox (state, next_attempt_at);
