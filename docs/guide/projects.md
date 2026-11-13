# Projects

A project is a named workspace of a profile that can span several folders
or repositories. Sessions started inside a project's folders are grouped
under it, and one project can be active: new desktop and TUI sessions
that were not given a directory start in its primary folder. A project is
data in the profile's `state.db`; activating or switching one never
changes a running session's system prompt or tools.

## Creating and listing

```console
$ prometheus projects create "Prometheus Agent" ~/src/prometheus ~/src/docs --use
prometheus-agent
$ prometheus projects list
* prometheus-agent Prometheus Agent <home>/src/prometheus
$ prometheus projects show prometheus-agent
active yes
folders <home>/src/prometheus (primary)
<home>/src/docs
$ prometheus projects show nope # exits 66
error: no project 'nope' in this profile
```

`list` (alias `ls`) shows live projects with the active one marked `*`;
`--all` adds archived ones.

- The slug derives from the name (`Prometheus Agent` -> `prometheus-agent`)
  unless `--slug` gives one (`^[a-z0-9][a-z0-9_-]{0,63}$`). A derived slug
  that is taken is numbered (`tools-2`); an explicit one that is taken
  fails (exit 1).
- The first folder is the primary one unless `--primary <path>` names
  another. Folders must exist; they are stored absolute and canonical.
- `--description`, `--icon`, `--color` and `--board <slug>` set the other
  fields; `--use` makes the project active.
- `<project>` is the id (`prj_...`) or the slug everywhere; an unknown one
  exits 66. `list` and `show` accept `--json`
  (`prometheus.projects_list.v1`, `prometheus.project.v1`).

## Folders and ownership

```console
$ prometheus projects create api ~/src/api
api
$ prometheus projects add-folder api ~/src/api-docs --label docs
added a folder to project 'api'
$ prometheus projects set-primary api ~/src/api-docs
set the primary folder of project 'api'
$ prometheus projects remove-folder api ~/src/api-docs
removed a folder from project 'api'
$ prometheus projects rename api "Public API"
renamed project 'api'
$ prometheus projects bind-board api backend
bound project 'api'
$ prometheus projects add-folder prometheus-agent ~/src/api # exits 1
error: folder <home>/src/api already belongs to project 'api'
```

`add-folder --primary` makes the new folder primary; `rename` keeps the
slug; `bind-board` without a board unbinds.

A folder belongs to at most one non-archived project: adding a folder
another project holds fails with `folder <path> already belongs to project
'<p>'` (exit 1). Removing the primary folder promotes the remaining folder
added first.

The project of a path is the non-archived project with the longest folder
that contains it, compared on whole path components: with `mono` on
`/src` and `svc` on `/src/svc`, a session in `/src/svc/lib` belongs to
`svc`, and one in `/src/svc-old` to `mono`. Every new session records the
project of its working directory once, when it is created;
`prometheus sessions list --project <project>` lists them.

## The active project

`prometheus projects use <project>` sets the profile's active project and
`prometheus projects use` clears it.

```console
$ prometheus projects use api
activated project 'api'
$ prometheus projects use
no project is active
```
 Desktop and TUI sessions created
without a directory start in its primary folder (the profile workspace
when the folder no longer exists); other surfaces are unaffected.

## Archive and restore

`prometheus projects archive <project>` hides a project from listings and
releases its folders for ownership; its sessions and folder rows are kept,
and archiving the active project clears the active pointer.
`prometheus projects restore <project>` brings it back, failing with
`folder <path> now belongs to project '<p>'` (exit 1) when another project
claimed one of its folders meanwhile.

```console
$ prometheus projects archive api
archived project 'api'
$ prometheus projects restore api
restored project 'api'
```

## Repository discovery

With `projects.repo_scan_enabled` on (unset: on for the desktop app, off
for every other surface), the daemon scans `projects.repo_scan_roots`
(default: your home directory) down to `projects.repo_scan_max_depth`
(default 4) levels for git repositories, in the background and without
following symlinks. It skips `projects.repo_scan_exclude_paths` subtrees
(`~` is your home), hidden folders, dependency folders (`node_modules`,
`target`, `.venv`, `vendor`) and folders it cannot read, and does not
look inside a repository for more. What it found is cached per profile in
`state.db` with when it was last seen; changing a scan key invalidates
only that profile's cache, and turning scanning off clears it.
Discovered repositories are only offered (the desktop's Projects sidebar
creates a project in one click, `projects.create`); no project is ever
created automatically.

## Backups and exports

Projects and the active pointer live in `state.db`, so whole-home backups
and quick snapshots carry them. `prometheus profile export` writes them to
the archive's `projects.json` even without `--with-sessions`, and `profile
import` writes them back with the same ids. Folders are imported as
recorded: a path that does not exist on the new machine stays in the
project and `projects show` flags it `missing`.
