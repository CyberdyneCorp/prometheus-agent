-- Schema v16 (spec: messaging-gateway, Per-chat policies; agent-interop,
-- One-shot send command). New nullable columns only: older binaries read
-- the tables unchanged.
--
-- `gateway_sessions.policy`: the chat policy frozen for the session when
-- it was created (JSON), so a policy edit applies to new sessions only.
-- `gateway_outbox.attachments`: files sent with the message (JSON array
-- of `{path, as_document}`), from `MEDIA:` directives.

ALTER TABLE gateway_sessions ADD COLUMN policy TEXT;

ALTER TABLE gateway_outbox ADD COLUMN attachments TEXT;
