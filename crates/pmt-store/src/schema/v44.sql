-- Schema v44 (spec: messaging-gateway, Cross-platform identity linking).
--
-- `gateway_identity_links`: sender accounts of different platforms that
-- one person linked with `/link`; each account belongs to at most one
-- person.
CREATE TABLE gateway_identity_links (
    platform   TEXT NOT NULL,
    sender_id  TEXT NOT NULL,
    person_id  TEXT NOT NULL,
    linked_at  INTEGER NOT NULL,
    PRIMARY KEY (platform, sender_id)
) STRICT;

CREATE INDEX gateway_identity_links_by_person ON gateway_identity_links (person_id);

-- `gateway_link_codes`: the 6-digit codes `/link` hands out, one per
-- account, valid until `expires_at`.
CREATE TABLE gateway_link_codes (
    code        TEXT NOT NULL PRIMARY KEY,
    platform    TEXT NOT NULL,
    sender_id   TEXT NOT NULL,
    expires_at  INTEGER NOT NULL
) STRICT;
