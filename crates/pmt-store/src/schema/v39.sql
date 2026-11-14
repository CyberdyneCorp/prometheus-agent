-- Schema v39 (spec: projects, Repository discovery).
--
-- `discovered_repos`: the git repositories the profile's background scan
-- found, with when a scan last saw each. `settings` is the fingerprint of
-- the `projects.repo_scan_*` keys the rows were found with: a change of
-- any scan key invalidates the whole cache of this profile.
CREATE TABLE discovered_repos (
    path      TEXT    PRIMARY KEY,
    name      TEXT    NOT NULL,
    settings  TEXT    NOT NULL,
    last_seen INTEGER NOT NULL
) STRICT;
