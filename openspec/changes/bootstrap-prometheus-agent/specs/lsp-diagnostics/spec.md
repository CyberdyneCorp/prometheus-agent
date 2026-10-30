# LSP Diagnostics

## Purpose
LSP diagnostics runs real language servers (rust-analyzer, pyright, gopls, typescript-language-server, clangd and others) as managed background processes and feeds the semantic errors an edit introduced into the results of `write_file` and `patch`. The key decisions are that a language server never breaks or blocks a write beyond a small budget, that only diagnostics introduced by the edit are reported, and that servers which execute project code (build scripts, project interpreters, compiler plugins) start only in workspaces the user pointed Prometheus at or explicitly trusted. In this spec `<profile>` denotes the profile's home directory resolved from the session's ProfileScope.

## ADDED Requirements

### Requirement: Workspace gating
The system SHALL run language servers only when `lsp.enabled = true` (default), the session's backend descriptor declares `uses_host_filesystem = true`, and the edited file lies inside a git working tree whose root is not matched by `lsp.exclude_roots` (globs, `~` expanded, a bare path also matching everything below it); with `lsp.enabled = false` no server, watcher or background task SHALL exist; a non-list `lsp.exclude_roots` SHALL fail config validation with exit code 78.
**Milestone:** M2

#### Scenario: Gateway session in the home directory
- **GIVEN** a gateway session whose cwd is the user's home directory, not a git repository
- **WHEN** the model writes `notes.py`
- **THEN** no language server SHALL start and the result SHALL carry only the syntax check

### Requirement: Built-in server registry
The system SHALL ship a typed registry mapping file extensions and root markers to servers with ids at least `rust-analyzer`, `pyright`, `typescript-language-server`, `gopls`, `clangd`, `bash-language-server`, `yaml-language-server`, `dockerfile-language-server`, `vue-language-server`, `svelte-language-server`, `astro-language-server`, `intelephense`, `lua-language-server`, `terraform-ls`, `jdtls`, `kotlin-language-server`, `elixir-ls`, `zls`, `clojure-lsp`, `haskell-language-server`, `ocaml-lsp` and `prisma-language-server`, each declaring its install method (`npm`, `go`, `pip`, or `manual`) and whether it executes project code; a configured id that is neither built in nor declared as a custom server SHALL be a config error.
**Milestone:** M2

#### Scenario: Rust file
- **GIVEN** a trusted Cargo workspace with `rust-analyzer` available
- **WHEN** the model patches `src/lib.rs`
- **THEN** diagnostics SHALL be requested from `rust-analyzer`

### Requirement: Workspace trust
A workspace SHALL be trusted only when it is the git working tree of the directory the session was pointed at (launch directory, `--cwd`, a `-w` worktree, a desktop or TUI project, or a gateway's `terminal.cwd`) or lies under an entry of `lsp.trusted_workspaces`; a nested checkout with its own `.git` and any repository at or above the user's home directory SHALL NOT inherit trust, a `cd` in the terminal SHALL NOT move trust, and cron jobs and kanban workers SHALL get no automatic trust; in an untrusted workspace only servers that execute no project code SHALL start, with project-code hooks pinned off (pyright using the managed Python instead of the project's virtualenv, typescript-language-server using the TypeScript bundled with the server, svelte-language-server with `isTrusted = false`), every other server and every custom server SHALL be skipped with the log line `skipped: untrusted workspace <root>; add it to lsp.trusted_workspaces`, and project-toolchain lint fallbacks (`npx tsc`, `rustfmt --check`) SHALL be skipped too.
**Milestone:** M2

#### Scenario: Freshly cloned repository
- **GIVEN** a session launched in `~/work/app` and the agent cloning `https://example.com/x.git` into `/tmp/x`
- **WHEN** the model edits `/tmp/x/build.rs`
- **THEN** `rust-analyzer` SHALL NOT start for `/tmp/x` and the skip SHALL be logged

#### Scenario: Explicit trust
- **GIVEN** `lsp.trusted_workspaces = ["~/code"]`
- **WHEN** the model edits a file under `~/code/lib/src/lib.rs`
- **THEN** `rust-analyzer` MAY start for that workspace

### Requirement: Language servers run sandboxed
Every language server SHALL run as a child process under the OS sandbox with `fs.read` on its workspace, the toolchain paths and its install directory, `fs.write` only on its cache directory under `<profile>/cache/lsp/`, the child environment allowlist from `execution-environments`, and network only through the session egress policy; a server that exits or exceeds its budget SHALL never affect the write that triggered it.
**Milestone:** M2

#### Scenario: Server tries to write the workspace
- **WHEN** a language server attempts to write `target/` in the workspace
- **THEN** the sandbox SHALL deny the write and the edit's result SHALL be unaffected

### Requirement: Diagnostics introduced by an edit
After a successful write whose syntax check is clean, the system SHALL send the new content to the file's server and compare the resulting diagnostics with those for the pre-edit content, reporting only new ones as `lsp_diagnostics` in the form `<SEVERITY> [<line>:<col>] <message> [<code>] (<server>)`; the write SHALL wait for the verdict only while the whole tool call stays under 80 ms; a slower verdict SHALL be attached to the session's next tool result labelled `LSP diagnostics introduced by your earlier edit of <path>`, and if the model ends its turn while a check is pending the system SHALL wait up to `lsp.wait_timeout_ms` (default 5000) and deliver any findings as a new message before accepting the final answer, never rewriting earlier messages; a second edit of the same file before the first check finishes SHALL be compared against the first edit's baseline.
**Milestone:** M2

#### Scenario: Undefined name
- **GIVEN** a warm pyright for a trusted workspace
- **WHEN** the model patches `app.py` to call an undefined `foo()`
- **THEN** the result SHALL include `lsp_diagnostics` reporting the undefined name and no pre-existing diagnostics

#### Scenario: Cold server
- **GIVEN** a server still starting when the write completes
- **WHEN** its verdict arrives later
- **THEN** the diagnostics SHALL be attached to the next tool result with the earlier-edit label

### Requirement: Wait modes and timeouts
`lsp.wait_mode` SHALL be `document` (default: diagnostics for the edited file) or `full` (also request workspace diagnostics); the first request against a workspace whose server is not running SHALL use `lsp.warmup_timeout_ms` (default 0 = same as `wait_timeout_ms`); after a server fails to spawn or times out, its (server, workspace) pair SHALL be skipped for `lsp.broken_retry_secs` (default 0 = until `prometheus lsp restart` or process exit); servers idle for `lsp.idle_timeout_secs` (default 600, 0 = never) SHALL be shut down and respawned on demand.
**Milestone:** M2

#### Scenario: Broken server retry
- **GIVEN** `lsp.broken_retry_secs = 300` and a gopls that timed out
- **WHEN** a Go file is edited 2 minutes later
- **THEN** gopls SHALL NOT be contacted, and an edit after 5 minutes SHALL retry it

### Requirement: Server installation
Missing server binaries SHALL be handled by `lsp.install_strategy`: `auto` (default) installs `npm`-, `go`- and `pip`-method servers on first use into `<PROMETHEUS_HOME>/runtimes/lsp/` using the managed Node and CPython runtimes and `lsp.package_manager` (`npm` default, `pnpm` or `yarn`), subject to `runtimes.allow_lazy_install`; `manual` uses only binaries on `PATH`; a configured package manager that is not installed SHALL skip the install with a notice and never fall back to another manager; `manual`-method servers SHALL never be installed automatically.
**Milestone:** M2

#### Scenario: pnpm policy
- **GIVEN** `lsp.package_manager = "pnpm"` and pnpm not installed
- **WHEN** pyright is needed
- **THEN** the install SHALL be skipped with a notice and npm SHALL NOT be used

### Requirement: Server overrides and custom servers
`lsp.servers.<id>` SHALL accept `disabled` (bool), `command` (argv, bypassing installation), `env` (non-secret map) and `initialization_options` (merged into the server's initialization options) for a built-in id; a key that is not a built-in id SHALL declare a custom server requiring `command` and `extensions` and accepting `root_markers`, `language_id` and `description`, matched before built-ins, installed manually only, and treated as executing project code for trust.
**Milestone:** M2

#### Scenario: Custom server
- **GIVEN** `lsp.servers.mylang = { command = ["mylang-ls", "--stdio"], extensions = [".ml2"] }` in a trusted workspace
- **WHEN** the model writes `a.ml2`
- **THEN** diagnostics SHALL be requested from `mylang-ls`

### Requirement: LSP CLI
`prometheus lsp status` SHALL list running servers with workspace, pid, uptime and state, and mark servers limited to trusted workspaces; `prometheus lsp list [--installed-only]` SHALL list registry entries with install state; `prometheus lsp install <id>` and `install-all` SHALL install `npm`/`go`/`pip` servers and print manual instructions for others; `prometheus lsp restart [<id>]` SHALL stop servers and clear broken marks; `prometheus lsp which <id>` SHALL print the resolved binary path and exit 66 when none is found; RPC `lsp.status` SHALL expose the same data to the desktop and dashboard.
**Milestone:** M2

#### Scenario: Which missing server
- **WHEN** `prometheus lsp which gopls` runs on a machine without gopls
- **THEN** it SHALL exit 66
