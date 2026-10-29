# CLI

## Purpose
The CLI is the `prometheus` binary's command-line surface: top-level subcommands, the interactive chat REPL, one-shot and piped runs, the slash-command registry shared by every surface, themes, the setup wizard, logs, diagnostics and shell completions. The key decisions are: one typed slash-command registry whose entries declare per-surface availability, busy policy and effect class (so the prompt-cache invariant is enforced by the registry, not by each handler), stable exit codes and `--json` output for scripting, and the CLI acting as a client of the local gateway daemon when one runs, with an in-process fallback.

## ADDED Requirements

### Requirement: Binary names
The system SHALL install the binary as `prometheus` with the alias `pmt`, and both names SHALL accept identical arguments and behave identically.
**Milestone:** M1

#### Scenario: Alias equivalence
- **WHEN** the user runs `pmt --version` and `prometheus --version`
- **THEN** both SHALL print the same `prometheus <semver> (<target>, <commit>)` line and exit 0

### Requirement: Top-level command tree
The system SHALL provide exactly the top-level subcommands listed by `workspace-architecture`, each arriving with its owning capability's milestone: M1 — `chat` (the default when no subcommand is given), `setup`, `config`, `secrets`, `grants`, `security`, `profile`, `sessions`, `model`, `auth`, `providers`, `tools`, `skills`, `memory`, `plugins`, `mcp`, `runtimes`, `usage`, `logs`, `status`, `doctor`, `trajectory`, `replay`, `demo`, `update`, `uninstall`, `bundle`, `migrate`, `debug`, `completions`, `version`, `help`; M2 — `daemon` (process lifecycle), `gateway` (messaging adapters and outbox), `tui`, `pairing`, `identity`, `cron`, `pause`, `curator`, `bridge`, `cache`, `files`, `schema`, `rpc`, `eval`; M3 — `dashboard`, `acp`, `kanban`, `evolve`, `harness`, `experience`; subcommand groups SHALL use plural nouns (for example `plugins test-adapter`, never `plugin`). It SHALL accept the global flags `-p/--profile`, `--home`, `--set key=value` (repeatable), `--json`, `--no-color`, `-v/--verbose` (repeatable), `-q/--quiet`, `-V/--version` and `-h/--help` before or after the subcommand.
**Milestone:** M1

#### Scenario: Default subcommand
- **WHEN** the user runs `prometheus -p work` in a terminal
- **THEN** the system SHALL start an interactive chat in profile `work`

#### Scenario: Unknown subcommand
- **WHEN** the user runs `prometheus sesions`
- **THEN** the system SHALL exit 2 printing `unknown command 'sesions' (did you mean 'sessions'?)`

### Requirement: Chat flags
The `chat` command SHALL accept `-m/--model`, `--provider`, `-t/--toolsets <list>`, `-r/--resume <ref>`, `-c/--continue` (resume the most recent session started from the CLI in the current directory), `-z/--oneshot <prompt>`, `--approvals <manual|smart|off>`, `--max-iterations <n>`, `--cwd <dir>` and `--in-process`, where per-invocation flags apply to the started session only and are never persisted.
**Milestone:** M1

#### Scenario: Continue in current directory
- **GIVEN** two CLI sessions, the most recent started in `/repo/a` and an older one in `/repo/b`
- **WHEN** the user runs `prometheus -c` from `/repo/b`
- **THEN** the system SHALL resume the session started in `/repo/b`

### Requirement: Stable exit codes
The system SHALL use exactly the exit-code table owned by `workspace-architecture`: 0 success; 1 failure; 2 usage error; 3 budget exhausted (for example a one-shot run that ended without a final answer because a turn or cost budget was exhausted); 4 approval or confirmation required but unobtainable non-interactively; 5 partial result; 10 update available (`update --check` only); 65 invalid input data; 66 not found (for example a missing input file); 69 unavailable (provider, daemon, runtime, or self-update of an install owned elsewhere); 70 internal error; 73 cannot create output; 74 I/O error; 75 temporary failure or lock held (rate limit, busy store); 76 protocol mismatch; 77 permission denied or strict security posture failure; 78 configuration or credential error; 130 SIGINT; 143 SIGTERM.
**Milestone:** M1

#### Scenario: Missing provider configuration
- **GIVEN** a fresh profile with no model configured
- **WHEN** the user runs `prometheus -z "hi"` non-interactively
- **THEN** the process SHALL exit 78 and print `no model configured; run: prometheus setup model`

### Requirement: One-shot and piped mode
The system SHALL run non-interactively when `-z/--oneshot` is given or when stdin is not a terminal, reading the prompt from the flag or from stdin, writing only the final answer to stdout (or with `--json` a single object with `session_id`, `answer`, `usage` and `exit_reason`), writing progress and logs to stderr, never starting the TUI, and denying any approval request in `manual` mode with exit 4 unless `--approvals` permits it.
**Milestone:** M1

#### Scenario: Piped prompt
- **WHEN** the user runs `cat notes.md | prometheus -z "summarise" > out.txt`
- **THEN** `out.txt` SHALL contain only the final answer text

#### Scenario: Approval in unattended run
- **GIVEN** `approvals.mode = "manual"`
- **WHEN** a one-shot run's tool call requires approval
- **THEN** the tool call SHALL be denied, the run SHALL end, and the process SHALL exit 4

### Requirement: Interface selection is TTY-first
The system SHALL decide the interactive interface by first checking that both stdin and stdout are terminals, and only then consulting `--tui`/`--line` and `ui.interface` (`"tui"` or `"line"`, default `"tui"` on an interactive TTY; M1 ships `line` only, so `tui` falls back to `line` until the TUI capability lands); a non-terminal stdin or stdout SHALL always select one-shot mode regardless of flags or config.
**Milestone:** M1

#### Scenario: Config cannot force TUI into a pipe
- **GIVEN** `ui.interface = "tui"`
- **WHEN** the user runs `echo hi | prometheus`
- **THEN** the system SHALL run in one-shot mode

### Requirement: Daemon attachment with in-process fallback
The interactive CLI SHALL attach to the running gateway daemon of the home as a client when one is running and serves the selected profile, SHALL otherwise run the agent in-process, SHALL run in-process when `--in-process` is given, and SHALL answer commands whose state lives in the daemon (session handoff, gateway status, reload) through the daemon connection.
**Milestone:** M2

#### Scenario: Session shared with gateway
- **GIVEN** a running daemon serving profile `default`
- **WHEN** the user starts `prometheus chat`
- **THEN** the session SHALL be created by the daemon and visible to `prometheus sessions list` and the dashboard while it runs

### Requirement: Process identity from runtime records
The system SHALL identify Prometheus daemons and clients through runtime records in `<home>/run/` (JSON with pid, process start time, version, role and served profiles, plus an exclusive lock file), SHALL treat a record whose pid is dead or whose start time differs as stale, and SHALL NOT classify processes by scanning command-line strings.
**Milestone:** M1

#### Scenario: Stale record after crash
- **GIVEN** `run/gateway.json` naming a pid that now belongs to an unrelated process with a different start time
- **WHEN** the user runs `prometheus daemon status`
- **THEN** the system SHALL report the gateway as not running and remove the stale record

### Requirement: Single typed slash-command registry
The system SHALL define every slash command exactly once in a registry whose entries carry `name`, `aliases`, `category`, `description`, typed `args` schema, `surfaces` (subset of `cli`, `tui`, `gateway`, `desktop`, `dashboard`, `acp`), `busy_policy` (`reject` default, `queue`, `interrupt_then_dispatch`, `immediate`) and `effect` (`none`, `immediate`, `next_session`, `new_session`), and SHALL fail registry construction (a build-time test for built-ins, a load error for plugins and skills) on a duplicate name or alias or an unknown enum value.
**Milestone:** M1

#### Scenario: Plugin command collides with built-in
- **GIVEN** a plugin declaring slash command `/model`
- **WHEN** the plugin loads
- **THEN** its command registration SHALL fail with `slash command '/model' conflicts with built-in` and the built-in SHALL remain

### Requirement: Command resolution
The system SHALL resolve slash input by lowercasing only the command token, accepting an optional leading `/`, matching names and aliases exactly, then expanding a token that is a prefix of exactly one available name, listing the candidates when a prefix is ambiguous, and otherwise printing `Unknown command: /<token>` with up to 3 suggestions within edit distance 2.
**Milestone:** M1

#### Scenario: Unique prefix
- **WHEN** the user types `/comp`
- **THEN** the system SHALL run `/compress`

#### Scenario: Arguments keep their case
- **WHEN** the user types `/TITLE My Project`
- **THEN** the title SHALL be set to `My Project`

### Requirement: Precedence for non-built-in slash commands
The system SHALL resolve a slash token in the order built-in commands, user quick commands, plugin commands, skill commands, and SHALL log a WARN at load naming both sources when a quick command, plugin command and skill share a name.
**Milestone:** M1

#### Scenario: Skill shadowed by quick command
- **GIVEN** a quick command `deploy` and a skill `deploy`
- **WHEN** the user types `/deploy`
- **THEN** the quick command SHALL run and the startup log SHALL contain one WARN naming both

### Requirement: Built-in slash command set
The system SHALL provide at least `/help`, `/new` (alias `/reset`), `/clear`, `/stop`, `/undo`, `/retry`, `/rewind`, `/fork`, `/resume`, `/sessions`, `/title`, `/model`, `/tools`, `/skills`, `/memory`, `/compress`, `/usage`, `/config`, `/theme`, `/approvals`, `/profile` (read-only display), `/verbose` and `/exit` (alias `/quit`), and `/help` SHALL list the commands available on the current surface grouped by category, generated from the registry.
**Milestone:** M1

#### Scenario: Gateway-unavailable command hidden from help
- **GIVEN** `/theme` declares surfaces `cli` and `tui` only
- **WHEN** a messaging user sends `/help`
- **THEN** `/theme` SHALL NOT be listed and sending `/theme` SHALL reply `/theme is not available here`

### Requirement: Busy policy enforcement
The system SHALL dispatch a resolvable slash command typed while a turn is running according to its `busy_policy` and SHALL never queue it as user text; a `reject` command SHALL print `/<name> is unavailable while the agent is working; use /stop first`, and `/stop` and `/new` SHALL be `interrupt_then_dispatch`.
**Milestone:** M1

#### Scenario: Rejected while busy
- **GIVEN** a running turn
- **WHEN** the user types `/compress`
- **THEN** the system SHALL print the unavailable message and the running turn SHALL continue unaffected

### Requirement: Deferred effects with `--now`
The system SHALL apply a command whose `effect` is `next_session` (including `/skills install|uninstall|enable|disable`, `/tools enable|disable`, `/memory reload` and personality or identity changes) by persisting the change and printing `<what> in your next session. Use /new to start one now, or add --now to apply immediately (invalidates prompt cache).`, and with `--now` SHALL apply the change to the current session at the next turn boundary by rebuilding its system prompt, tool set or memory snapshot and recording one `prompt.rebuilt` event with `reason` `<command> --now`; without `--now` the current session's system prompt, tool set and memory snapshot SHALL never change.
**Milestone:** M1

#### Scenario: Toolset change deferred
- **WHEN** the user types `/tools disable web` during a session
- **THEN** the current session's next request SHALL still include the web tools and the next session SHALL not

#### Scenario: Toolset change now
- **WHEN** the user types `/tools disable web --now`
- **THEN** the same session's next request SHALL exclude the web tools and the trajectory SHALL contain one `prompt.rebuilt` event with `reason` `tools --now`

### Requirement: Destructive command confirmation
The system SHALL ask `once / always / cancel` before `/clear`, `/new`, `/undo` and `/rewind` when `cli.confirm_destructive = true` (default), SHALL persist `cli.confirm_destructive = false` on `always`, SHALL leave the session unchanged on `cancel`, and SHALL skip the prompt when the command carries `--yes` or `-y`.
**Milestone:** M1

#### Scenario: Cancel keeps history
- **WHEN** the user types `/clear` and answers `cancel`
- **THEN** the session transcript SHALL be unchanged

### Requirement: Skill invocation as user message
The system SHALL invoke `/<skill> [instruction]` by appending a user message that loads the skill, never by editing the system prompt, and SHALL accept up to 5 stacked leading skill tokens (`/a /b do X`), rejecting a sixth with `at most 5 skills per message`.
**Milestone:** M1

#### Scenario: Stacked skills
- **WHEN** the user types `/pdf /summarise report.pdf`
- **THEN** one user message SHALL be appended that loads both skills followed by the instruction `report.pdf`, and the system prompt hash SHALL be unchanged

### Requirement: Quick commands
The system SHALL support `[quick_commands.<name>]` entries with `type = "alias"` (re-dispatch `target`, appending user arguments) or `type = "exec"` (run `command` through the local sandbox with a default 30 s timeout set by `timeout_secs`, an environment stripped of every secret, and redacted output), and SHALL reject any other `type` or an alias that resolves to itself at config validation.
**Milestone:** M1

#### Scenario: Exec quick command timeout
- **GIVEN** an exec quick command `slow` running `sleep 60`
- **WHEN** the user types `/slow`
- **THEN** the command SHALL be killed after 30 s and print `quick command 'slow' timed out after 30s`

### Requirement: Interactive input handling
The REPL SHALL support multi-line input (Alt+Enter or a trailing `\` continues the line), persistent per-profile history in `<profile>/cli_history` capped at 10000 entries with secrets redacted, Ctrl+C interrupting a running turn and, when idle, a second Ctrl+C within 2 s or Ctrl+D exiting, and `@path` completion for files under the session's working directory.
**Milestone:** M1

#### Scenario: Ctrl+C interrupts the turn, not the process
- **GIVEN** a running turn
- **WHEN** the user presses Ctrl+C once
- **THEN** the turn SHALL stop, the partial transcript SHALL be persisted, and the REPL SHALL return to the prompt

### Requirement: Single interactive owner per session
The system SHALL refuse a second interactive client resuming a session that another interactive client holds the turn lease for, printing `session <id> is open in another client (pid <n>)` and offering to follow it read-only or fork it.
**Milestone:** M2

#### Scenario: Second terminal offered a fork
- **GIVEN** a session open in one terminal
- **WHEN** the user runs `prometheus -r <id>` in another terminal and chooses fork
- **THEN** a new session forked at the last message SHALL open in the second terminal

### Requirement: Themes are data
The system SHALL ship built-in themes `default`, `mono`, `daylight` and `slate`, SHALL load user themes from `<profile>/themes/<name>.toml` with optional `extends = "<built-in>"` and partial overrides, SHALL ignore a user theme whose name equals a built-in with a WARN, SHALL select the theme from `display.theme` (default `default`), and SHALL disable colour when `--no-color` is given, `NO_COLOR` is set, or stdout is not a terminal.
**Milestone:** M1

#### Scenario: Unknown theme
- **WHEN** the user types `/theme neon` and no theme `neon` exists
- **THEN** the system SHALL list the available themes and keep the current one

### Requirement: Setup wizard
The system SHALL provide `prometheus setup [model|secrets|tools|terminal|memory|gateway]` that shows current values and writes only through the config and secrets writers, SHALL offer setup automatically when an interactive `prometheus` starts on a profile with no model configured, and SHALL support `--non-interactive` driven by `--set` flags and secrets from the environment, exiting 78 when a required value is missing.
**Milestone:** M1

#### Scenario: Re-run is idempotent
- **GIVEN** a configured profile
- **WHEN** the user runs `prometheus setup model` and accepts every default shown
- **THEN** `config.toml` SHALL be byte-identical afterwards

### Requirement: Log viewer
This capability SHALL own the log viewer: `prometheus logs [<component>|list] [-n/--lines N] [-f/--follow] [--level L] [--since S] [--session <id>|--root <id>] [--json]`, where `<component>` (default `agent`) is one of `agent`, `errors`, `gateway`, `adapter`, `cron`, `kanban`, `mcp`, `plugins`, `sandbox`, `providers`, `rpc`, `update`, `migrate`, `evolve`, `eval`, `-n` defaults to 50 records, `--since` accepts `<N>[smhd]` or an RFC 3339 timestamp, `--session` filters to one session and `--root` to every session of one root lineage; it SHALL read profile logs from `<profile>/logs/` and `gateway`, `update` and `migrate` from `<home>/logs/`, SHALL print the path of every file it reads in its header, and SHALL exit 2 naming the allowed values for an unknown component, level or invalid `--since`; log files SHALL rotate at 10 MiB keeping 5 files.
**Milestone:** M1

#### Scenario: Header names the real file
- **WHEN** the user runs `prometheus -p work logs update`
- **THEN** the header SHALL name `<home>/logs/update.log`

### Requirement: Doctor
The CLI SHALL expose `prometheus doctor` exactly as owned by `workspace-architecture` (`prometheus doctor [<group>] [--fix] [--online] [--strict] [--json]`, one check-group list and one enumerated `--fix` set), checking among others config validity, schema version, required secrets for configured providers, state store integrity and quarantine records, sandbox enforcement strength on this OS, daemon status, managed runtime presence and file permissions, printing one line per check as `ok`, `warn` or `fail` with a remedy, and exiting 1 when any check fails.
**Milestone:** M1

#### Scenario: Missing secret reported
- **GIVEN** `model.provider = "anthropic"` and no `ANTHROPIC_API_KEY` in the profile
- **WHEN** the user runs `prometheus doctor`
- **THEN** the output SHALL contain a `fail` line naming `ANTHROPIC_API_KEY` and `prometheus secrets set ANTHROPIC_API_KEY`, and the exit code SHALL be 1

### Requirement: Machine-readable output
The system SHALL make every listing and status command accept `--json`, emitting a single JSON document (or JSON Lines for `--follow` streams) to stdout with no decoration, using `snake_case` field names and RFC 3339 timestamps, and SHALL emit errors under `--json` as `{"error": {"code": "<code>", "message": "<text>"}}` on stdout alongside the exit code.
**Milestone:** M1

#### Scenario: JSON error
- **WHEN** the user runs `prometheus sessions show nope --json`
- **THEN** stdout SHALL be `{"error":{"code":"session.not_found","message":...}}` and the exit code SHALL be 1

### Requirement: Shell completions (upgrade)
The system SHALL provide `prometheus completions <bash|zsh|fish|powershell|elvish>` printing a completion script generated from the command tree, with dynamic completion of profile names, session ids and titles, config keys, skill names and slash commands served by the hidden command `prometheus __complete`, which SHALL read only local files, SHALL NOT start the daemon or open network connections, and SHALL return within 200 ms on a store of 10000 sessions.
**Milestone:** M2

#### Scenario: Config key completion
- **WHEN** the user types `prometheus config set agent.max<TAB>` in zsh with completions installed
- **THEN** the shell SHALL complete `agent.max_iterations`

### Requirement: Usage display
The system SHALL show, on `/usage` and `prometheus sessions stats <ref>`, input, output, cache-read, cache-write and reasoning tokens and cost for the current session and for its whole lineage (including compression continuations, forks and delegated children), plus remaining turn and cost budget when configured.
**Milestone:** M1

#### Scenario: Lineage cost shown
- **GIVEN** a session compressed once and with one delegated child
- **WHEN** the user types `/usage`
- **THEN** the lineage total SHALL include the continuation's and the child's costs
