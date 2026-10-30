# Projects

## Purpose
Projects are per-profile, human-named workspaces that can span several folders or repositories. They group sessions on every surface, give kanban tasks a deterministic worktree and branch convention when bound to a board, and let GUI sessions switch workspace explicitly. The key decisions are that a project is data in the profile's state store (never ambient state), that switching a project changes a session's working directory but never its system prompt or tool set, and that folder ownership is resolved deterministically by longest path prefix.

## ADDED Requirements

### Requirement: Project records
The system SHALL store projects in the profile's `state.db` with `id` (`prj_<ULID>`), `slug` (`^[a-z0-9][a-z0-9_-]{0,63}$`, unique per profile, derived from the name unless given), `name`, optional `description`, `icon` and `color`, `primary_path`, optional bound kanban `board`, `created_at` and `archived`, plus folder rows (`path` canonicalised and absolute, optional `label`, `is_primary`), with at most one primary folder per project and a folder path belonging to at most one non-archived project.
**Milestone:** M2

#### Scenario: Folder claimed twice
- **GIVEN** project `api` containing `/src/api`
- **WHEN** the user adds `/src/api` to project `web`
- **THEN** the command SHALL fail with `folder /src/api already belongs to project 'api'` and exit 1

### Requirement: Project commands
The system SHALL provide `prometheus projects create <name> [<folder>...] [--slug <s>] [--primary <path>] [--description <t>] [--icon <i>] [--color <c>] [--board <slug>] [--use]` (first folder is primary unless `--primary`), `list [--all]` (alias `ls`; `--all` includes archived), `show <project>`, `add-folder <project> <path> [--label <l>] [--primary]`, `remove-folder <project> <path>`, `rename <project> <name>`, `set-primary <project> <path>` (path must already be a folder), `use [<project>]` (omit to clear), `archive <project>`, `restore <project>` and `bind-board <project> [<board>]` (omit to unbind), resolving `<project>` by id or slug, exiting 66 for an unknown project, and accepting `--json` on listings.
**Milestone:** M2

#### Scenario: Create and activate
- **WHEN** the user runs `prometheus projects create "Prometheus Agent" ~/src/prometheus ~/src/docs --use`
- **THEN** `prometheus projects show prometheus-agent` SHALL list both folders with `~/src/prometheus` as primary and mark the project active

### Requirement: Active project and path ownership
The system SHALL keep one active project per profile (stored in the state store), SHALL resolve the project owning a path as the non-archived project with the longest folder path that is a prefix of it on path-component boundaries, SHALL record that project's id on each new session from its `cwd` (so `sessions list --project <slug>` and surface sidebars group by it), and SHALL use the active project's primary folder as the default working directory of new GUI sessions that were not given a directory.
**Milestone:** M2

#### Scenario: Nested folders
- **GIVEN** project `mono` with folder `/src` and project `svc` with folder `/src/svc`
- **WHEN** a session starts in `/src/svc/lib`
- **THEN** the session SHALL be recorded under project `svc`

### Requirement: Archive and restore
Archiving a project SHALL hide it from default listings and pickers, release its folders for path ownership, and keep its sessions and folder rows; restoring SHALL fail with `folder <path> now belongs to project '<p>'` when any of its folders was claimed meanwhile, and archiving the active project SHALL clear the active pointer.
**Milestone:** M2

#### Scenario: Archived active project cleared
- **GIVEN** active project `api`
- **WHEN** the user runs `prometheus projects archive api`
- **THEN** no project SHALL be active and `prometheus projects list` SHALL NOT show `api`

### Requirement: Kanban board binding
A project bound to a kanban board SHALL give each `worktree` task on that board created without an explicit branch the deterministic branch `<project-slug>/<task-id>[-<title-slug>]` (title slug lowercase, characters outside `[a-z0-9._-]` collapsed to `-`, at most 40 characters) and a worktree rooted in the project's primary folder, overriding the board default `kanban/<task_id>`, and binding to a board that does not exist SHALL fail with exit 66.
**Milestone:** M3

#### Scenario: Branch convention
- **GIVEN** project `api` bound to board `backend`
- **WHEN** task `t_1a2b3c4d` titled `Fix Login Bug` is created with workspace `worktree`
- **THEN** its branch SHALL be `api/t_1a2b3c4d-fix-login-bug`

### Requirement: Project tool for GUI sessions
The system SHALL provide a `project` toolset, enabled by default only for `desktop` and `tui` surfaces, containing the tool `project` with actions `list` (projects with the calling session's own project marked), `create` (`name`, optional `path`; idempotent when `path` already belongs to a project, returning it), and `switch` (`project` id or slug), where `switch` and `create` move only the calling session's working directory to the project's primary folder at the next tool call and notify the surface, never change the system prompt, tool set or the profile's active project for other sessions, and are never triggered implicitly by a `cd` in the terminal tool; the tool SHALL declare `effect = workspace`.
**Milestone:** M3

#### Scenario: Switch keeps prompt cache
- **GIVEN** a desktop session in project `api`
- **WHEN** the model calls `project` with `action = "switch"` and `project = "web"`
- **THEN** the session's next terminal command SHALL run in `web`'s primary folder and the next provider request SHALL have the same system prompt hash

### Requirement: Repository discovery
The system SHALL, when `projects.repo_scan_enabled = true` (default true on desktop, false elsewhere), scan `projects.repo_scan_roots` (default empty, meaning the user's home directory) to depth `projects.repo_scan_max_depth` (default 4) for git repositories in the background without following symlinks, skipping `projects.repo_scan_exclude_paths` subtrees, hidden directories, dependency directories (`node_modules`, `target`, `.venv`, `vendor`) and paths the sandbox cannot read, cache results per profile in the state store with `last_seen`, never create projects automatically, and offer discovered repositories as one-click project creation in the desktop Projects sidebar and dashboard; setting `repo_scan_enabled = false` SHALL stop scanning and clear that profile's discovery cache while keeping explicit projects and repositories inferred from sessions, and changing any scan key SHALL invalidate only that profile's cache.
**Milestone:** M3

#### Scenario: Excluded path skipped
- **GIVEN** `projects.repo_scan_exclude_paths = ["~/scratch"]` and a repository at `~/scratch/tmp`
- **WHEN** a scan completes
- **THEN** `~/scratch/tmp` SHALL NOT be listed as discovered

### Requirement: Projects in backups and exports
Project records SHALL be included in whole-home backups, quick snapshots and `prometheus profile export`, and on `profile import` into another machine folders whose paths do not exist SHALL be kept and flagged `missing` in `projects show` instead of being dropped.
**Milestone:** M2

#### Scenario: Missing folder flagged
- **GIVEN** an exported profile with project folder `/home/a/src/x`
- **WHEN** it is imported on a machine without that path
- **THEN** `prometheus projects show x` SHALL list `/home/a/src/x` as `missing`
