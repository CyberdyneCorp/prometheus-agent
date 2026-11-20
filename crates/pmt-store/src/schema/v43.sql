-- Schema v43 (spec: session-storage, Hybrid semantic and full-text
-- search).
--
-- `message_embeddings`: a derived index of active user and assistant
-- messages, one vector (little-endian f32) per message and embedding
-- model, with the sha256 of the text it was computed from. Filled in the
-- background, never inside a turn; a hybrid search fuses it with the word
-- index. Rows of deleted messages are dropped by the next fill.
CREATE TABLE message_embeddings (
    message_id  TEXT NOT NULL,
    model       TEXT NOT NULL,
    content_sha TEXT NOT NULL,
    vector      BLOB NOT NULL,
    PRIMARY KEY (message_id, model)
) STRICT, WITHOUT ROWID;

CREATE INDEX message_embeddings_by_model ON message_embeddings (model);
