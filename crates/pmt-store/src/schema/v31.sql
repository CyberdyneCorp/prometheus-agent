-- Schema v31 (spec: context-compression, Pinned messages survive
-- compaction).
--
-- The messages a user pinned with `/pin`: the text is captured verbatim at
-- pin time, so a pin outlives the compaction that archives its row and is
-- copied into the `## Pinned` section of every later summary. Rows go with
-- their session.
CREATE TABLE session_pins (
    session_id TEXT    NOT NULL REFERENCES sessions (id),
    message_id TEXT    NOT NULL,
    text       TEXT    NOT NULL,
    pinned_at  INTEGER NOT NULL,
    PRIMARY KEY (session_id, message_id)
) STRICT;
