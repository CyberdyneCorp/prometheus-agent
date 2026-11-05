# Undo, checkpoints and verification

Prometheus keeps what the agent's file tools change, so a turn can be taken
back, and checkpoints of the projects it works in, so a workspace can be
rolled back. Everything lives in the profile's snapshot store,
`<profile>/snapshots/`: file contents (content-addressed, shared),
per-project checkpoints and per-session journals.

## What is recorded

With `files.snapshot.enabled = true` (default; `prometheus chat
--checkpoints` or `--no-checkpoints` overrides it for one run):

- before every [`write_file`](../reference/tools.md#write_file) and
  [`patch`](../reference/tools.md#patch) change (including deletes and
  renames of a V4A patch) the file's prior content, or its absence, is
  stored with the session, turn and call id;
- at most once per project per turn, before the first file-tool write in
  it, a **checkpoint** of the whole project: every file its `.gitignore`
  does not exclude (`.git`, nested repositories and the Prometheus home left
  out; a nested git repository is a project of its own). Files larger than
  `files.snapshot.max_file_bytes` (default 10 MiB) are listed as skipped,
  not stored; each project keeps its newest `files.snapshot.max_checkpoints`
  (default 20) checkpoints;
- with `files.snapshot.terminal = "destructive"` (default `off`), also a
  checkpoint before terminal commands shaped like `rm`, `rmdir`, `cp`, `mv`,
  `install`, `sed -i`, `truncate`, `dd`, `shred`, output redirection `>`,
  `git reset`, `git clean`, `git checkout` and `git restore`.

Recording never fails or blocks the call it records. Sessions on other
terminal backends are snapshotted too:

- a `container` session whose workspace is mounted
  (`terminal.container.mount_workspace`, the default) is recorded on the
  host: its `/workspace/...` paths are the host project's files, so the
  journal, the checkpoints, `/undo`, `/rollback` and `prometheus files`
  all work on the host paths;
- a session whose files live only in the backend (SSH, a plugin backend, a
  container without the mount) is read, checkpointed and restored through
  that backend's file operations: a checkpoint costs one backend call per
  directory and per file (turn `files.snapshot.enabled` off for a huge
  remote project). Those checkpoints and edits belong to that backend: the
  host's `prometheus files undo` reports them as
  `conflict: <path> is on the <backend> backend; undo it from a session on that backend`
  instead of touching a host path of the same name.

## `/undo`

`/undo [n] [--force]` removes the last `n` exchanges (default 1) from the
conversation and puts back every file the file tools changed in those
turns: prior content restored, created files removed. A file whose content
differs from what the session last wrote (you edited it since) is refused
with `conflict: <path> changed since turn <n>; use --force to overwrite`,
and nothing is marked undone until `--force` is given. A note recording the
undo is appended to the conversation. The same is available as `prometheus
files undo --session <id> [--turn <n>] [--dry-run] [--force]` and the RPC
method `files.undo`.

## `/rollback`

- `/rollback` lists the checkpoints of the session workspace, newest first:
  number, time, triggering tool, and files and lines changed since.
- `/rollback diff <N>` shows the unified diff between checkpoint `N` and now;
  nothing changes.
- `/rollback <N>` restores the files changed since checkpoint `N` (and
  removes files created since). A file you edited by hand after the agent's
  last write to it is kept and reported as `kept (hand-edited)`; `--all`
  overwrites it too. `/rollback <N> <file>` restores only that file. A note
  marking the latest turn as undone is appended to the conversation.

The same operations are `prometheus files rollback [N] [FILE] [--diff]
[--all] [--dir <DIR>]` and the RPC method `files.rollback`.

## The snapshot store

- `prometheus files snapshots status` (alias `list`, `--limit <n>`, default
  20): total size, workspace count, and per workspace its size, checkpoint
  count and last use (`(missing)` when its directory is gone).
- `prometheus files snapshots prune [--retention-days <n>] [--max-mb <n>]
  [--keep-orphans]`: deletes workspaces unused for longer than the retention
  (default 7 days), cuts the store to the size cap oldest checkpoint first
  (default `files.snapshot.max_bytes`), and deletes workspaces whose
  directory is missing unless `--keep-orphans`.
- `prometheus files snapshots clear [-f]` deletes the whole store after a
  typed confirmation; without a terminal to confirm it exits 4.

```console
$ prometheus files snapshots status
snapshot store: 0 bytes, 0 workspaces
$ prometheus files snapshots clear   # exits 4
$ prometheus files snapshots prune --keep-orphans
pruned 0 workspaces, 0 checkpoints, 0 journals; freed 0 bytes
```

Sessions prune automatically in the background at most once per
`files.snapshot.prune_interval_secs` (default a day), with
`files.snapshot.retention_secs` and `files.snapshot.max_bytes`; the
automatic prune never deletes a workspace whose directory is missing (it may
be on an unmounted volume).

## `/diff`

`/diff [unstaged|staged|all|session] [--stat] [<path>...]` shows, for the
session directory's git repository, the unstaged changes plus untracked
files (default), the staged changes, or both; `session` shows the cumulative
diff of every file the agent changed in this session from its earliest
snapshot, and works outside git too. `--stat` gives per-file counts only.
On a messaging platform the diff is fenced and cut to the platform's message
length with a note giving how many lines were left out. Outside a repository
the git modes answer `not a git repository`.

## `prometheus verify`

`prometheus verify [PATH] [--detect-only] [--save] [--phase
bootstrap|build|test|start]... [--skip-start] [--port <n>] [--timeout
<secs>] [--ready-timeout <secs>] [--json]` detects the project's recipe:
`.prometheus/environment.json` when present, else from `Cargo.toml` (`cargo
build`, `cargo test`), `package.json` (install, `build`, `test` and `start`
scripts with the package manager of its lockfile), `pyproject.toml` (uv,
Poetry or pip), `go.mod` or a `Makefile`. `--detect-only` prints the recipe
as JSON and runs nothing; `--save` writes it to `environment.json`.

The phases run in order on the profile's terminal backend, each bounded by
`--timeout` (default 600 s): locally under the OS sandbox (the project
writable, the network allowed); on `container` in the container (the project
mounted as `/workspace`; a run from a session shares that session's
container, a run from a shell gets one of its own, removed afterwards unless
persistent); on `ssh` or a plugin backend in its workspace (the recipe is
still detected from `PATH` on this host). A backend that cannot be set up
fails the run; it never falls back to this host. `start` runs in the
background until its port (`--port` or the recipe's `port`) accepts
connections or `--ready-timeout` (default 60 s) passes, then is stopped; on a
non-local backend the port is probed inside it (`nc -z`, else bash's
`/dev/tcp`), so the backend needs one of them.
Each phase prints its status and duration, and a failed one the tail of its
output. The exit code is 0 when every phase passed and 1 otherwise. Run from
a session's terminal (which sets `PROMETHEUS_SESSION_ID`), the outcome is
recorded in `state.db` as verification evidence of that session.
