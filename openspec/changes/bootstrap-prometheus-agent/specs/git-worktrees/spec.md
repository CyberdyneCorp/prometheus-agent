# Git Worktrees

## Purpose
Git worktrees give each agent session or subagent its own branch and checkout of a repository so parallel agents never step on each other, with `prometheus chat -w`, the in-session `/worktree` command and opt-in subagent isolation. The key decisions are that Prometheus-created trees are checked out without running the repository's own hooks, filters or fsmonitor; that unfinished work is never silently deleted (dirty or unpushed trees are archived to a git ref before removal and can be restored); and that moving a session into a worktree changes the tools' working directory without rebuilding the system prompt. In this spec `<profile>` denotes the profile's home directory resolved from the session's ProfileScope.

## ADDED Requirements

### Requirement: Worktree creation
Creating a Prometheus worktree SHALL fetch the repository's default remote (unless `worktrees.sync_remote = false`, default true; a fetch failure SHALL fall back to the local tip with a notice), create `<repo>/.worktrees/<name>` on a new branch `prometheus/<name>` based on the remote tip of the current branch's upstream or the default branch, add `.worktrees/` to the repository's `info/exclude`, and check it out with hooks disabled (`core.hooksPath` pointed at an empty directory), `core.fsmonitor` off and clean/smudge filters and LFS smudging off for that checkout; an unnamed tree SHALL be named `prom-<8 hex>`, a name SHALL match `[A-Za-z0-9._-]{1,64}`, and an existing name SHALL fail with `worktree '<name>' already exists`.
**Milestone:** M2

#### Scenario: Repository with a post-checkout hook
- **GIVEN** a repository whose `post-checkout` hook writes a file
- **WHEN** a Prometheus worktree is created
- **THEN** the hook SHALL NOT run

#### Scenario: Unnamed tree
- **WHEN** a worktree is created without a name
- **THEN** it SHALL be created at `.worktrees/prom-<8 hex>` on branch `prometheus/prom-<8 hex>`

### Requirement: chat -w worktree mode
`prometheus chat -w/--worktree [NAME]` SHALL create a worktree for the repository containing the launch directory and run the whole session with it as cwd and trusted workspace; outside a git repository it SHALL exit 2 with `--worktree requires a git repository`; it SHALL combine with `-z` and with resume of a session that already owns a worktree, which SHALL reopen that tree.
**Milestone:** M2

#### Scenario: One-shot in a worktree
- **WHEN** the user runs `prometheus chat -w -z "Fix issue #123"` inside a repository
- **THEN** the agent SHALL run with cwd `<repo>/.worktrees/prom-<hex>` and the main checkout SHALL be unchanged

### Requirement: Exit disposition
When an interactive session that owns a worktree ends, the system SHALL remove a clean tree whose commits are all pushed or merged, and for a tree with uncommitted changes or unpushed commits SHALL ask `keep` (default, also on Enter or Esc), `archive and remove` (archive first; keep the tree if archiving fails; print the restore command) or `remove and discard` (only when chosen explicitly); single-query runs, cron jobs, gateway sessions and exits without a terminal SHALL never prompt and SHALL keep the tree for the sweep.
**Milestone:** M2

#### Scenario: Ctrl-D with uncommitted work
- **GIVEN** an interactive `-w` session with an uncommitted edit
- **WHEN** the user exits and presses Enter at the prompt
- **THEN** the worktree SHALL be kept unchanged

#### Scenario: Gateway session
- **GIVEN** a gateway session owning a dirty worktree
- **WHEN** the session ends
- **THEN** no prompt SHALL be sent and the tree SHALL be kept

### Requirement: In-session worktree command
`/worktree new [NAME]` SHALL create a worktree and retarget the session's terminal, file tools, snapshots and LSP trust to it without restarting the session, announcing the new cwd in the command's reply and in the next tool result rather than by changing the system prompt or tool array; `/worktree` SHALL show the active tree and branch, `/worktree list` SHALL list Prometheus trees of the repository, and `/worktree prune [--dry-run]` SHALL run the sweep for the repository; the same operations SHALL be exposed as RPC `worktree.new`, `worktree.status`, `worktree.list` and `worktree.prune`.
**Milestone:** M2

#### Scenario: Prompt cache preserved
- **GIVEN** an ongoing session in `/repo`
- **WHEN** the user runs `/worktree new exp`
- **THEN** the next model request SHALL carry the same system prompt and tools as before, and `terminal(command="pwd")` SHALL print `/repo/.worktrees/exp`

### Requirement: Repository registry and sweep schedule
Every repository in which Prometheus creates a worktree (chat `-w`, `/worktree new`, subagent isolation, the desktop git pane) SHALL be recorded in `<profile>/worktree_repos.json`; the system SHALL sweep each recorded repository's `.worktrees/` at `chat -w` start and every 6 hours from the scheduler, and SHALL drop registry entries whose repository no longer exists.
**Milestone:** M2

#### Scenario: Scheduled sweep
- **GIVEN** a recorded repository with an idle clean merged tree older than its tier
- **WHEN** the 6-hourly sweep runs
- **THEN** the tree SHALL be removed

### Requirement: Sweep decision table
The sweep SHALL use idle tiers of 24 hours for generated names (`prom-*`, `subagent-*`) and 72 hours for user-named trees, where idleness is the newest of file, `HEAD` and reflog activity; it SHALL never touch a tree with a live Prometheus lock owner or any live process whose cwd is inside it, a tree younger than its tier, or kanban task trees; past its tier it SHALL remove a clean merged-or-pushed tree, and archive then remove a dirty or unpushed tree when `worktrees.archive_dirty = true` (default; false keeps such trees); it SHALL keep, with the reason recorded, trees containing initialized submodules (`keep-submodule-work`), embedded repositories (`keep-unarchivable`), commits reachable only from per-worktree refs (`keep-worktree-refs`) or untracked files larger than `worktrees.archive_max_file_bytes` (default 52428800, `keep-large-untracked`); when `worktrees.max_total_bytes` (default 0 = off) is exceeded the oldest idle trees past their tier SHALL be archived and removed first.
**Milestone:** M2

#### Scenario: Live process inside
- **GIVEN** an idle-looking tree in which a dev server process has its cwd
- **WHEN** the sweep runs
- **THEN** the tree SHALL NOT be touched

#### Scenario: Large untracked file
- **GIVEN** a dirty tree containing an untracked 200 MB file
- **WHEN** the sweep evaluates it past its tier
- **THEN** the tree SHALL be kept with reason `keep-large-untracked`

### Requirement: Archive format
An archive SHALL be a commit at `refs/prometheus/archive/<tree>` capturing the tree's `HEAD`, staged changes, unstaged changes and non-ignored untracked files, built with a temporary index so the tree itself is never modified, storing file bytes exactly (no attribute, line-ending or filter conversion), and listing omitted oversized files in its message; if the archive cannot be built, or the tree is entered or its index changes during the build, the tree SHALL be kept; after archiving, a branch that exists on a remote SHALL be kept and otherwise deleted locally; archives older than `worktrees.archive_retention_secs` (default 2592000, 0 = forever) SHALL be deleted by the sweep.
**Milestone:** M2

#### Scenario: Index changes during archiving
- **GIVEN** a sweep archiving tree `lane-x`
- **WHEN** the user runs `git add` in `lane-x` mid-archive
- **THEN** the archive SHALL be saved and the tree SHALL be kept

### Requirement: Worktree CLI and restore
`prometheus worktrees list [--json]` SHALL list live trees and archives with name, age, branch and last decision reason; `prometheus worktrees prune [--dry-run] [--repo <path>]` SHALL run the sweep and with `--dry-run` print every archive, remove and keep decision with its reason while changing nothing; `prometheus worktrees restore <name> [--path <dir>]` SHALL recreate the tree (default `.worktrees/<name>`) with its branch, staged and untracked files, restoring the branch when missing or still at the archived `HEAD` and otherwise leaving the tree detached at that commit; an unknown archive SHALL exit 66.
**Milestone:** M2

#### Scenario: Restore an archived lane
- **GIVEN** an archive `refs/prometheus/archive/lane-x` with staged and untracked files
- **WHEN** the user runs `prometheus worktrees restore lane-x`
- **THEN** `.worktrees/lane-x` SHALL exist with the same staged changes and untracked files

### Requirement: Shared install state retention
Dependency install state cached per checkout outside `.worktrees/` and the profile data root (for example package-manager stores keyed by checkout) SHALL be deleted by the sweep when its checkout no longer exists and it has been unused for `worktrees.install_state_retention_secs` (default 1209600, 0 = never).
**Milestone:** M3

#### Scenario: Deleted checkout
- **GIVEN** install state for a checkout deleted 20 days ago
- **WHEN** the sweep runs
- **THEN** that install state SHALL be removed

### Requirement: Subagent worktree isolation
When `delegation.worktree_isolation = true` (default false) and the parent's backend is `local` inside a git repository, each child SHALL run in its own worktree `<repo>/.worktrees/subagent-<id>` on branch `<delegation.worktree_branch_prefix>/subagent-<id>` (prefix default `prometheus`); the child's result SHALL carry the `worktree` object defined by `subagent-delegation`; on finish a clean tree with no new commits SHALL be removed only after a successful inspection probe, and a tree with commits or edits SHALL be archived and removed with its branch kept for the parent to merge (kept in place instead when `worktrees.archive_dirty = false` or archiving fails); on other backends or outside git, isolation SHALL be skipped with a note in the child result rather than failing the delegation.
**Milestone:** M2

#### Scenario: Child commits work
- **GIVEN** worktree isolation on and a child that commits a fix
- **WHEN** the child finishes
- **THEN** the result SHALL name branch `prometheus/subagent-<id>` with its commits and the tree directory SHALL be removed

#### Scenario: Container backend
- **GIVEN** worktree isolation on and `terminal.backend = "container"`
- **WHEN** a child is spawned
- **THEN** it SHALL run in the parent's workspace and its result SHALL note that isolation was skipped
