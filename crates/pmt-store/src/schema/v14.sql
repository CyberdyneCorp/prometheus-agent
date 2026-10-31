-- Schema v14 (spec: cli, Ignore profile config and rules; Onboarding
-- state). New tables only: older binaries read the rest unchanged.
--
-- `session_launch_flags`: the launch flags a session was started with
-- (`ignore_user_config`, `ignore_rules`) so `sessions show` reports them.
-- `onboarding`: first-run progress of the profile (`setup_offered`,
-- one-time tips by id, completed tour steps); times are Unix ms.

CREATE TABLE session_launch_flags (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    flag       TEXT NOT NULL CHECK (flag IN ('ignore_user_config', 'ignore_rules')),
    PRIMARY KEY (session_id, flag)
) WITHOUT ROWID;

CREATE TABLE onboarding (
    kind TEXT    NOT NULL CHECK (kind IN ('setup_offered', 'tip', 'tour')),
    id   TEXT    NOT NULL CHECK (length(id) > 0),
    at   INTEGER NOT NULL,
    PRIMARY KEY (kind, id)
) WITHOUT ROWID;
