-- Schema v41 (spec: harness-evolution, Harness loaded once per session).
--
-- `sessions.harness_version`: the harness version (`h<n>-<fingerprint>`)
-- the session resolved from `<profile>/harness/ACTIVE` at its start. A
-- resumed session keeps it, so adopting or rolling back a harness never
-- changes an open session's prompt, skills or subagents.
ALTER TABLE sessions ADD COLUMN harness_version TEXT;
