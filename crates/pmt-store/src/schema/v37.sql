-- Schema v37 (spec: projects, Project records; Active project and path
-- ownership; Archive and restore).
--
-- `projects`: human-named workspaces of the profile. `slug` is unique per
-- profile (archived projects keep theirs). `board` is the bound kanban
-- board's slug. The primary path is the folder row with `is_primary = 1`
-- (at most one per project, enforced by the partial unique index).
-- A folder belongs to at most one NON-archived project; archived projects
-- keep their rows, so that rule is checked by the writer, not an index.
CREATE TABLE projects (
    id          TEXT    PRIMARY KEY,
    slug        TEXT    NOT NULL UNIQUE,
    name        TEXT    NOT NULL,
    description TEXT,
    icon        TEXT,
    color       TEXT,
    board       TEXT,
    created_at  INTEGER NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1))
) STRICT;

CREATE TABLE project_folders (
    project_id TEXT    NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path       TEXT    NOT NULL,
    label      TEXT,
    is_primary INTEGER NOT NULL DEFAULT 0 CHECK (is_primary IN (0, 1)),
    added_at   INTEGER NOT NULL,
    PRIMARY KEY (project_id, path)
) STRICT;
CREATE UNIQUE INDEX project_folders_primary ON project_folders (project_id) WHERE is_primary = 1;
CREATE INDEX project_folders_path ON project_folders (path);

-- The project owning the session's cwd when it was created; sessions
-- outlive their project's deletion.
ALTER TABLE sessions ADD COLUMN project_id TEXT REFERENCES projects(id) ON DELETE SET NULL;
CREATE INDEX sessions_project ON sessions (project_id, last_activity_at);
