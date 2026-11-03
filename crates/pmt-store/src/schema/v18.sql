-- Schema v18 (spec: messaging-gateway, Gateway rate limits and cost caps).
--
-- `gateway_turns`: who sent each gateway turn. A daily cost cap is keyed
-- by profile and canonical sender, and a shared thread or chat session
-- runs turns for many senders, so spend is attributed per turn (its calls
-- and its subagents' calls until the session's next turn), not to the
-- user who started the session.

CREATE TABLE gateway_turns (
    session_id TEXT    NOT NULL,
    started_at INTEGER NOT NULL,
    platform   TEXT    NOT NULL,
    sender_id  TEXT    NOT NULL,
    PRIMARY KEY (session_id, started_at)
) STRICT;

CREATE INDEX gateway_turns_sender ON gateway_turns (platform, sender_id);
