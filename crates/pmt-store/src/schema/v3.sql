-- Schema v3: trajectory retention bookkeeping. The retention sweep deletes
-- an ended session's trajectory file and records when (spec:
-- observability, Trajectory retention and size limits). A nullable column
-- only: stores of v2 binaries read it unchanged.

ALTER TABLE sessions ADD COLUMN trajectory_purged_at INTEGER;
