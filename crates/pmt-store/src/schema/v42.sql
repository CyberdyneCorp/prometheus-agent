-- Schema v42 (spec: experience-library, Domain resolution for a job;
-- Lessons offered to later sessions).
--
-- `sessions.domain`: the session's experience-library domain tag, set by
-- `prometheus chat --domain`, `/domain <name>`, or the scheduled job or
-- kanban task that started it. An evolution job of the session inherits it.
-- `sessions.lessons`: the lessons layer frozen at the session's start
-- (empty text when none was offered), so a prompt rebuild never picks up a
-- lesson adopted while the session was open.
ALTER TABLE sessions ADD COLUMN domain TEXT;
ALTER TABLE sessions ADD COLUMN lessons TEXT;
