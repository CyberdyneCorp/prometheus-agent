# Git worktrees

A git worktree is a second checkout of a repository on its own branch.
Prometheus uses them so parallel agents never edit the same files: a chat
started with `-w` works in its own tree, `/worktree new` moves a running
session into one, and delegated children can each get one
(`delegation.worktree_isolation`). Unfinished work is never deleted
silently: a tree with uncommitted changes or unpushed commits is either
kept or saved to an archive you can restore.

## Where trees live

A tree of the repository at `<repo>` is created at
`<repo>/.worktrees/<name>` on a new branch `prometheus/<name>`:

- an unnamed tree is called `prom-<8 hex>`; a name must match
  `[A-Za-z0-9._-]{1,64}` and must not be taken
  (`worktree '<name>' already exists`, exit 65);
- the default remote is fetched first and the branch starts at the remote
  tip of the current branch's upstream (or the remote's default branch).
  `worktrees.sync_remote = false` skips the fetch; a failed fetch starts
  from the local tip and says so;
- `.worktrees/` is added to the repository's `.git/info/exclude`;
- the checkout runs none of the repository's own code: `core.hooksPath`
  points at an empty directory, `core.fsmonitor` is off and every
  configured clean/smudge filter (and LFS smudging) is disabled. A
  `post-checkout` hook does not run.

Every repository Prometheus makes a tree in is recorded in
`<profile>/worktree_repos.json`, which the sweep below works through.

## `chat -w`

```text
prometheus chat -w                # a new prom-<hex> tree
prometheus chat -w fix-login      # .worktrees/fix-login on prometheus/fix-login
prometheus chat -w -z "Fix issue #123"
prometheus chat -w -r <session>   # reopen the tree that session owns
```

The whole session runs with the tree as its working directory and trusted
workspace; the main checkout is untouched. Outside a git repository `-w`
exits 2 with `--worktree requires a git repository`. Resuming a session
that owns a tree reopens it (restoring it from its archive when the sweep
archived it meanwhile). Every `-w` start sweeps the recorded repositories.

When an interactive session ends (`/exit`, Ctrl-D):

- a clean tree whose commits are all pushed or merged is removed with its
  branch;
- a tree with uncommitted changes or unpushed commits asks
  `[k]eep (default), [a]rchive and remove, [r]emove and discard`. Enter,
  Esc or anything else keeps it; `a` archives it first (and keeps it when
  archiving fails) and prints the `prometheus worktrees restore` command;
  only an explicit `r` discards work.

One-shot runs (`-z`, piped input), cron jobs, messaging sessions and exits
without a terminal never ask: the tree is kept for the sweep.

## `/worktree` in a session

| Command | What it does |
|---|---|
| `/worktree` | the active tree and branch |
| `/worktree new [NAME]` | creates a tree and moves the session into it |
| `/worktree list` | the repository's Prometheus trees and archives |
| `/worktree prune [--dry-run]` | runs the sweep on the repository |

`/worktree new` rebuilds the session's terminal, file tools, snapshots and
language-server trust for the tree without restarting the session. The
system prompt and the tool list stay exactly as they were, so the
provider's prompt cache survives; the move is announced in the command's
reply and at the start of the next tool result instead. The old terminal's
background processes end. Resuming the session later reopens the tree.
Clients use the same operations as `worktree.new`, `worktree.status`,
`worktree.list` and `worktree.prune`.

## The sweep

The sweep runs at every `chat -w` start and every 6 hours in the daemon,
over each recorded repository (entries whose repository is gone are
dropped). For each Prometheus tree it decides:

| Situation | Decision |
|---|---|
| a live Prometheus session works in it, or any process has its working directory inside it | never touched |
| idle less than its tier: 24 h for `prom-*` and `subagent-*`, 72 h for named trees (idle = newest file, `HEAD`, reflog or index change) | kept (`recent-activity`) |
| a kanban task's tree | never touched |
| clean, every commit pushed or merged | removed |
| dirty or unpushed, `worktrees.archive_dirty = true` (default) | archived, then removed |
| dirty or unpushed, `archive_dirty = false` | kept (`keep-dirty`) |
| initialized submodules | kept (`keep-submodule-work`) |
| an untracked nested repository | kept (`keep-unarchivable`) |
| commits only `refs/worktree/*` or `refs/bisect/*` reach | kept (`keep-worktree-refs`) |
| an untracked file over `worktrees.archive_max_file_bytes` (50 MiB) | kept (`keep-large-untracked`) |

With `worktrees.max_total_bytes` set, trees kept only because archiving is
off are archived and removed, oldest idle first, until the trees fit.
Archives older than `worktrees.archive_retention_secs` (30 days; `0`
keeps them forever) are deleted. A removed tree's branch is kept when it
exists on a remote and deleted otherwise (its commits are in the archive).

```console
$ prometheus worktrees prune --dry-run
no recorded repositories
```

## Archives and restore

An archive is the commit `refs/prometheus/archive/<tree>`, shaped like
`git stash -u`: the tree's `HEAD`, its staged changes, its unstaged
changes and its non-ignored untracked files. It is built in a temporary
index, so the tree is never modified, and stores file bytes exactly (no
line-ending, attribute or filter conversion). Oversized untracked files
are left out and listed in the archive's message. If the tree's index or
`HEAD` changes while the archive is built, or someone enters the tree,
the archive is saved but the tree is kept.

```text
prometheus worktrees list [--repo <path>] [--json]
prometheus worktrees prune [--dry-run] [--repo <path>]
prometheus worktrees restore <name> [--path <dir>]
```

`restore` recreates the tree (default `.worktrees/<name>`) with its
staged changes, unstaged changes and untracked files. Its branch is
restored when it is missing or still at the archived `HEAD`; otherwise
the tree is left detached at that commit. An unknown archive exits 66.

## Subagent isolation

With `delegation.worktree_isolation = true` and a session on the local
terminal backend inside a git repository, each delegated child runs in
`<repo>/.worktrees/subagent-<id>` on branch
`<delegation.worktree_branch_prefix>/subagent-<id>` (prefix `prometheus`),
started at the parent's `HEAD`, and is told to commit its work there. The
child's sandbox may write the repository's shared `objects`, `refs` and
`logs` and the tree's own git directory, never its hooks or configuration.
When the child finishes, its result carries
`worktree: {path, branch, commits, dirty, pruned, archive_ref, inspection_failed}`:

- no edits and no commits: the tree and branch are removed
  (`pruned: true`) once inspecting the tree succeeded;
- commits or edits: the tree is archived and removed and its branch kept
  for the parent to merge (kept in place when `worktrees.archive_dirty`
  is off or archiving fails);
- a tree that cannot be inspected is kept in place
  (`inspection_failed: true`).

On another backend (container, SSH, a plugin) or outside git the child
shares the parent's workspace and its result's `note` says isolation was
skipped and why; the delegation never fails for it.
