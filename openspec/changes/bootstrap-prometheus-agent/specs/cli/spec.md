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
The system SHALL provide exactly the top-level subcommands listed by `workspace-architecture`, each arriving with its owning capability's milestone: M1 — `chat` (the default when no subcommand is given), `setup`, `config`, `secrets`, `grants`, `profile`, `sessions`, `model`, `fallback`, `auth`, `tools`, `toolsets`, `approvals`, `skills`, `memory`, `runtimes`, `usage`, `logs`, `doctor`, `prompt`, `trajectory`, `demo`, `backup`, `snapshots`, `update`, `debug`, `version`, `help`; M2 — `daemon` (process lifecycle: `run`, `start`, `stop`, `restart`, `status`, `ensure`, `install-service`, `uninstall-service`, `ticket`, `pair-device`, `revoke-device`), `gateway` (messaging adapters and outbox: `status`, `setup`, `platforms`, `list`, `pause`, `resume`, `reload`, `restart`, `stop`, `outbox`, `send`, `peer`, `proxy`, `enroll`, `selftest`, `slack-manifest`, `whatsapp pair`, `whatsapp-cloud setup`, `photon setup|status`, `telegram logout`, `feishu comment-rules`), `tui`, `console`, `webhooks`, `pairing`, `cron`, `pause`, `curator`, `plugins`, `mcp`, `bridge`, `providers`, `portal`, `local`, `moa`, `connectors`, `browser`, `vault`, `security`, `status`, `lsp`, `verify`, `worktrees`, `projects`, `journey`, `themes`, `cache`, `files`, `schema`, `rpc`, `replay`, `eval`, `migrate`, `bundle`, `uninstall`, `completions`; M3 — `identity`, `dashboard` (`register`, `pair`, `devices`), `serve`, `webapp`, `desktop` (alias `gui`), `acp`, `computer-use`, `egress`, `codex-runtime`, `kanban`, `evolve`, `harness`, `experience`, `pets`, `achievements`; subcommand groups that manage a collection of installed or stored items SHALL use plural nouns (for example `plugins test-adapter`, never `plugin`; likewise `sessions`, `skills`, `worktrees`, `projects`), while the established singular groups listed in the tree above (such as `profile`, `config`, `model`, `daemon`, `gateway`, `vault`) keep their names, nested subcommands are owned by the capability that introduces the group, and `prometheus help` SHALL additionally list the commands of enabled plugins under `plugins run <plugin_id>`. It SHALL accept the global flags `-p/--profile`, `--home`, `--set key=value` (repeatable), `--json`, `--no-color`, `-v/--verbose` (repeatable), `-q/--quiet`, `-V/--version` and `-h/--help` before or after the subcommand.
**Milestone:** M1

#### Scenario: Default subcommand
- **WHEN** the user runs `prometheus -p work` in a terminal
- **THEN** the system SHALL start an interactive chat in profile `work`

#### Scenario: Unknown subcommand
- **WHEN** the user runs `prometheus sesions`
- **THEN** the system SHALL exit 2 printing `unknown command 'sesions' (did you mean 'sessions'?)`

### Requirement: Chat flags
The `chat` command SHALL accept `-m/--model`, `--provider`, `-t/--toolsets <list>`, `-r/--resume <ref>`, `-c/--continue` (resume the most recent session started from the CLI in the current directory), `-z/--oneshot <prompt>`, `--approvals <manual|smart|off>`, `--max-iterations <n>`, `--cwd <dir>`, `--in-process`, `--image <path>` and `--ignore-user-config`, plus the flags owned by other capabilities — `-w/--worktree [NAME]` (`git-worktrees`), `--checkpoints`/`--no-checkpoints` (`execution-environments`), `-s/--skills <list>` (`skills`), `--bot-chat` (`bot-mode`), `--domain <name>` (`experience-library`), `--egress <mode>` and `--allow-host <pattern>` (`security-boundaries`), `--base-url` and `--api-key-stdin` (`model-providers`), `--yolo` (`tool-system`), `--safe-mode` (`plugin-system`), `--source <tag>` (`session-storage`), `--pass-session-id` and `--run-budget` (`agent-turn-loop`) and `--mock-script` (`replay-and-evaluation`) — where per-invocation flags apply to the started session only and are never persisted.
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
The system SHALL provide at least `/help`, `/new` (alias `/reset`), `/clear`, `/stop`, `/undo`, `/retry`, `/rewind`, `/fork`, `/resume`, `/sessions`, `/title`, `/model`, `/tools`, `/skills`, `/memory`, `/compress`, `/usage`, `/config`, `/theme`, `/approvals`, `/profile` (read-only display), `/verbose` and `/exit` (alias `/quit`), and the registry SHALL also hold, each arriving with its owner's milestone and defined there, `/background` (alias `/bg`), `/queue`, `/prompt` (aliases `/compose`, `/edit`), `/image`, `/timestamps` (alias `/ts`), `/focus`, `/export` and `/import` (`cli`); `/usage reset` and `/reasoning`, `/fast`, `/provider`, `/subscription` (alias `/upgrade`), `/topup` (`model-providers`); `/rollback`, `/diff`, `/browser`, `/sync` (`execution-environments`); `/worktree` (`git-worktrees`); `/cron`, `/suggestions` (alias `/suggest`) and `/blueprint` (alias `/bp`) (`scheduled-jobs`); `/kanban` (`kanban`); `/learn`, `/refine`, `/bundles`, `/reload-skills`, `/curator` and one dynamic `/<bundle>` per skill bundle (`skills`); `/journey` (`memory`); `/goal`, `/subgoal`, `/loop`, `/heartbeat` (`session-automation`); `/snapshot` (`configuration-and-profiles`); `/save`, `/autoprune` (`session-storage`); `/compress`, `/context`, `/pin`, `/unpin` (`context-compression`); `/insights`, `/debug`, `/budget` (`observability`); `/plugins` and `/mcp` (`plugin-system`); `/moa` (`mixture-of-agents`); `/evolve` (`harness-evolution`); `/domain` (`experience-library`); `/delegation` (`subagent-delegation`); `/egress` (`security-boundaries`); `/yolo` (`tool-system`); `/personality` (`agent-turn-loop`); `/voice` and `/wake` (`voice-mode`); `/pet` (`pets`); `/platforms` (alias `/gateway`), `/platform`, `/handoff`, `/branch`, `/footer`, `/topic`, `/link`, `/unlink`, `/busy`, `/pause`, `/policy`, `/sethome`, `/commands`, `/whoami`, `/pair`, `/restart` and `/update` (`messaging-gateway`); and `/help` SHALL list the commands available on the current surface grouped by category, generated from the registry.
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
The system SHALL show, on `/usage` and `prometheus sessions stats <ref>`, input, output, cache-read, cache-write and reasoning tokens and cost for the current session and for its whole lineage (including compression continuations, forks and delegated children), plus remaining turn and cost budget when configured, and, for a subscription route, the account usage block and `/usage reset` defined by `model-providers` › Account usage windows command.
**Milestone:** M1

#### Scenario: Lineage cost shown
- **GIVEN** a session compressed once and with one delegated child
- **WHEN** the user types `/usage`
- **THEN** the lineage total SHALL include the continuation's and the child's costs

### Requirement: Resume and continue resolution
The system SHALL resolve `-r/--resume <ref>` by session id, id prefix, title (per `session-storage` › Title lineage numbering and resolution) or the keyword `latest` (the most recent user-facing session in the current workspace), SHALL treat `@claude-code` and `@codex` (optionally `@claude-code:<FILE>`) as a request to import that agent's most recent log for the current workspace through `hermes-migration` › Session transcripts from other agent CLIs and resume the resulting session, SHALL accept `-c/--continue [<name>]` where a bare `-c` resumes the most recent CLI or TUI session started in the current workspace and `-c <name>` resumes the most recent session in the lineage whose base title is `<name>`, SHALL prefer the session last used from the same terminal when `sessions.terminal_continue = true` (default true; the terminal is identified by its controlling TTY device and session leader, never by argv), SHALL restore the session's recorded `cwd` when it still exists (printing `cwd restored to <dir>`) unless `--cwd <dir>` is given, in which case `--cwd` changes directory before startup, scopes the `-c` and `latest` lookup to `<dir>` and overrides the restore, and SHALL exit 66 with `no session to continue in <dir>` when nothing matches.
**Milestone:** M1

#### Scenario: Continue by name picks newest numbered session
- **GIVEN** sessions titled `Refactor` and `Refactor #3`, both started in `/repo`
- **WHEN** the user runs `prometheus -c Refactor` from `/repo`
- **THEN** the system SHALL resume `Refactor #3`

#### Scenario: Per-terminal continue
- **GIVEN** `sessions.terminal_continue = true`, session A last used from terminal 1 and a newer session B last used from terminal 2, both in `/repo`
- **WHEN** the user runs `prometheus -c` in terminal 1
- **THEN** the system SHALL resume session A

#### Scenario: Explicit cwd overrides restore
- **GIVEN** session S recorded with cwd `/repo/a`
- **WHEN** the user runs `prometheus --resume S --cwd /repo/b`
- **THEN** the session's working directory SHALL be `/repo/b`

### Requirement: Conversation recap on resume
The system SHALL print a local recap when an interactive session is resumed, controlled by `display.resume_display` (`full` default, `minimal` printing only title, message count and last activity, or `off`), showing at most `display.resume_exchanges` (default 10) user/assistant exchanges, truncating user messages to `display.resume_max_user_chars` (default 300), non-final assistant messages to `display.resume_max_assistant_chars` (default 200) and `display.resume_max_assistant_lines` (default 3), showing the final assistant message in full, skipping tool-only assistant messages, and never sending the recap to the model.
**Milestone:** M1

#### Scenario: Tool-only messages skipped
- **GIVEN** a session whose last assistant message contained only tool calls followed by a final text answer
- **WHEN** it is resumed with `display.resume_display = "full"`
- **THEN** the recap SHALL show the final text answer and SHALL NOT show the tool-only message

### Requirement: Seed prompt and prompt file
The `chat` command SHALL accept `--query <text>`, which starts an interactive session and submits `<text>` verbatim as the first user turn (never interpreted as a slash command, `!` shell escape or `@` reference) and then keeps the session open, and `--prompt-file <path>` (`-` reads stdin), which reads the prompt verbatim as bytes decoded as UTF-8 for either the seed or the one-shot prompt; `--query`, `--prompt-file` and `-z` SHALL be mutually exclusive (exit 2), a missing file SHALL exit 66, and the global `-q/--quiet` SHALL suppress the banner, spinner and tool previews while still printing the answer.
**Milestone:** M1

#### Scenario: Seed text starting with a slash
- **WHEN** the user runs `prometheus --query "/model is what I want to discuss"`
- **THEN** the system SHALL send that exact text as the first user message and SHALL NOT run `/model`

#### Scenario: Untrusted body from a file
- **GIVEN** a file `body.txt` containing `!rm -rf ~`
- **WHEN** the user runs `prometheus -z --prompt-file body.txt`
- **THEN** the text SHALL be sent to the model as the prompt and no shell command SHALL run

### Requirement: Streaming JSONL output
The system SHALL accept `--output-format text|json|stream-json` on one-shot runs (`--json` being shorthand for `json`), where `stream-json` writes one JSON object per line to stdout as events happen using the trajectory event grammar owned by `observability` (the same line shape and type names as `observability` › Trajectory event types, at least `session.start`, `assistant.message`, `tool.call`, `tool.result`, `approval.request` and `turn.end`), ends with a `result` line carrying `session_id`, `answer`, `usage` and `exit_reason`, flushes after each line, is refused with exit 2 for interactive or TUI sessions, and keeps the documented exit codes.
**Milestone:** M2

#### Scenario: Stream ends with result
- **WHEN** the user runs `prometheus -z "list files" --output-format stream-json`
- **THEN** every stdout line SHALL parse as JSON and the last line SHALL have `type = "result"` with the run's `exit_reason`

### Requirement: Usage report file
The `chat` command SHALL accept `--usage-file <path>` and, when the run ends for any reason including failure or SIGINT, write atomically a JSON document with `session_id`, `root_session_id`, `model`, `provider`, `billing_source`, input, output, cache-read, cache-write and reasoning tokens, `cost` with `cost_status`, `turns`, `exit_reason` and `exit_code`, independent of stdout and of `--output-format`, exiting 73 when the file cannot be created.
**Milestone:** M1

#### Scenario: Usage written on failure
- **GIVEN** a one-shot run that fails with exit 69 because the provider is unreachable
- **WHEN** it was started with `--usage-file u.json`
- **THEN** `u.json` SHALL exist with `exit_code = 69` and zero output tokens

### Requirement: Image attachments
The system SHALL attach local images to a prompt through the repeatable `chat --image <path>` flag (applies to the first prompt) and the `/image <path>` slash command (applies to the next prompt; bare `/image` lists pending attachments, `/image clear` drops them), accepting PNG, JPEG, GIF and WebP up to 20 MiB each, storing them in the session blob store, and refusing with `model <id> does not accept images` when the resolved model metadata has `supports_vision = false` and no vision auxiliary route is configured.
**Milestone:** M1

#### Scenario: Non-vision model
- **GIVEN** a model whose metadata has `supports_vision = false` and no auxiliary vision model
- **WHEN** the user runs `prometheus -z "describe" --image shot.png`
- **THEN** the system SHALL exit 78 with `model <id> does not accept images` and SHALL NOT call the provider

### Requirement: Ignore profile config and rules
The `chat` command SHALL accept `--ignore-user-config`, which runs the session with schema defaults plus per-invocation flags instead of the profile's `config.toml` and managed values still applied (secrets still resolved), and `--ignore-rules`, which skips injection of workspace rule files (`AGENTS.md`, `.cursorrules` and equivalents), the identity file, the memory snapshot and preloaded skills, and both SHALL be recorded on the session so `prometheus sessions show` reports them.
**Milestone:** M1

#### Scenario: Rules skipped
- **GIVEN** a workspace containing `AGENTS.md`
- **WHEN** the user runs `prometheus --ignore-rules -z "hi"`
- **THEN** the system prompt SHALL NOT contain the `AGENTS.md` content and the session record SHALL show `ignore_rules = true`

### Requirement: Shell escape in the REPL
The interactive REPL SHALL treat a line beginning with `!` as a shell command run through the session's terminal backend under the same sandbox and approval rules as the `terminal` tool, showing the output locally, never sending it to the model unless the line begins with `!!` (which appends the command and its output, redacted and capped at 16 KiB, as a user-context message for the next turn), and SHALL reject `!` lines while a turn is running with `shell escape is unavailable while the agent is working`.
**Milestone:** M1

#### Scenario: Output kept local
- **WHEN** the user types `!git status`
- **THEN** the output SHALL be printed and the next model request SHALL NOT contain it

### Requirement: Job-control suspend
The REPL SHALL, on Ctrl+Z on POSIX terminals while idle, restore the terminal's original mode and raise SIGTSTP to itself, and on SIGCONT SHALL re-enter its input mode and redraw the prompt without losing the input buffer; while a turn is running Ctrl+Z SHALL be ignored with the hint `cannot suspend while the agent is working`.
**Milestone:** M1

#### Scenario: Resume after fg
- **GIVEN** an idle REPL with `half typed` in the input buffer
- **WHEN** the user presses Ctrl+Z and later runs `fg`
- **THEN** the prompt SHALL be redrawn with `half typed` still in the buffer

### Requirement: Tool progress display and focus view
The system SHALL render tool activity according to `display.tool_progress` (`off`, `new` showing a line only when the tool changes, `all` default for the CLI, `verbose` with arguments and result previews; default `off` for messaging adapters via `gateway.platforms.<platform>.display.tool_progress`), truncate previews to `display.tool_preview_length` characters (default 0, meaning no limit), group consecutive calls per `display.tool_progress_grouping` (`accumulate` default, `separate`), let `/verbose` cycle `off → new → all → verbose` for the current surface (in messaging only when `gateway.display.tool_progress_command = true`), and provide `/focus [on|off|status]`, which pins tool progress to `off`, prints one dim `N tool lines hidden` line per turn, shows a `focus` status badge and restores the previous mode on `off`; these settings SHALL be display-only and never change the prompt or tool set.
**Milestone:** M1

#### Scenario: Focus restores previous mode
- **GIVEN** `display.tool_progress = "verbose"`
- **WHEN** the user types `/focus on`, runs a turn with 4 tool calls, then types `/focus off`
- **THEN** the turn SHALL print `4 tool lines hidden` and the mode SHALL be `verbose` again

### Requirement: Display preferences
The system SHALL define in the config schema at least `display.bell_on_complete` (false), `display.bell_on_prompt` (false; rings when an approval or clarify prompt opens), `display.show_cost` (false; cost in the status line), `display.show_reasoning` (false), `display.final_response_markdown` (`render` default, `raw`), `display.inline_diffs` (true; file-edit diffs shown inline), `display.vim_mode` (false), `display.compact` (false), `display.turn_summary` (true; one line with duration, tool count and tokens after each turn), `display.interim_messages` (true; assistant text between tool calls), `display.timestamps` (false) and `display.timestamp_format` (`%H:%M`), and SHALL provide `/timestamps [on|off|status]` (alias `/ts`) toggling timestamps for the current surface and persisting the value.
**Milestone:** M1

#### Scenario: Bell on approval
- **GIVEN** `display.bell_on_prompt = true`
- **WHEN** an approval prompt opens in the REPL
- **THEN** the system SHALL write one BEL character to the terminal

### Requirement: Theme commands and branding keys
The system SHALL provide `prometheus themes list|use <name>|set <key> <value>|show` and SHALL allow theme files to define, besides colours, `spinner.frames` (list of strings), `spinner.phrases` (list of status phrases), `branding.agent_name`, `branding.prompt_symbol` and `branding.banner` (text, at most 40 lines), with `set` writing to a user theme `<profile>/themes/custom.toml` that extends the active theme; desktop and dashboard SHALL read the same theme file and MAY add `css` overrides under `[desktop]` that are ignored by terminal surfaces.
**Milestone:** M2

#### Scenario: Custom prompt symbol
- **WHEN** the user runs `prometheus themes set branding.prompt_symbol "λ"` and starts a chat
- **THEN** the REPL prompt SHALL start with `λ`

### Requirement: Setup wizard modes
The `setup` command SHALL also accept sections `tts` and `agent` (identity file, approval mode, iteration budget), `--quick` (prompt only for values that are unset), `--reset` (after confirmation, restore the section's keys to schema defaults while keeping secrets), and `--portal` (sign in to Nous Portal through `prometheus auth login nous`, set `model.provider = "nous"` with the portal's default model and route the web, image and TTS tool providers to `nous`, showing the resulting diff before writing); on first run it SHALL offer to import from a detected Hermes, OpenClaw, Claude Code, Codex or Cursor installation through `hermes-migration`; and `prometheus setup gateway [<platform>]` SHALL prompt for each field of the chosen adapter's config schema (secrets through the secrets writer) and offer to restart the daemon's adapter when it runs.
**Milestone:** M2

#### Scenario: Quick setup asks only unset values
- **GIVEN** a profile with a model configured and no TTS provider
- **WHEN** the user runs `prometheus setup --quick`
- **THEN** the wizard SHALL NOT ask for the model and SHALL ask for the TTS provider

#### Scenario: Adapter fields from schema
- **WHEN** the user runs `prometheus setup gateway telegram`
- **THEN** the wizard SHALL prompt for every required field of the Telegram adapter schema and store the bot token only in the secrets store

### Requirement: Safe command console
The system SHALL provide `prometheus console`, a REPL that executes only an allowlist of `prometheus` subcommands (`status`, `doctor`, `logs`, `sessions list|show|export`, `config show|get|validate`, `usage`, `version`, `update --check`, `daemon status|restart`, `gateway status`), parses each line with the CLI parser without invoking any shell, rejects other input with `not available in the console`, inherits the `-p` profile, and SHALL refuse `!` escapes, pipes and redirection.
**Milestone:** M2

#### Scenario: Shell metacharacters refused
- **WHEN** the user types `logs; rm -rf /` in the console
- **THEN** the console SHALL reject the line and run nothing

### Requirement: Webhook subscriptions
The system SHALL provide `prometheus webhooks subscribe <name> [--events <list>] [--skills <list>] [--deliver <target>] [--prompt <template>] [--secret-name <NAME>] [--deliver-only] [--script <path>] [--cron-job <id|name>] [--mirror-to-session]`, `list`, `remove <name>` and `test <name> [--payload FILE]` (flags map one-to-one onto the route fields of `platform-adapters` › Webhook routes, `platform-adapters` › Webhook payload filters and transform scripts and `platform-adapters` › Webhook direct delivery mode, and `scheduled-jobs` › Webhook-triggered job runs; `--prompt` templates reference payload fields as `{dot.path}`; `--mirror-to-session` also appends the delivered text to the target chat's session so follow-ups have context), persisting each route in the profile's state (not `config.toml`), generating a 32-byte random HMAC secret into the secrets store when `--secret-name` is not given and printing it once, exposing the route at `/webhooks/<profile>/<name>` on the webhook adapter, and SHALL have `test` deliver a signed sample payload through the same path, printing the resulting session id.
**Milestone:** M2

#### Scenario: Subscribe prints secret once
- **WHEN** the user runs `prometheus webhooks subscribe deploys --events push --deliver telegram:123`
- **THEN** the system SHALL store a new secret, print it once with the route URL, and `prometheus webhooks list` SHALL show the route without the secret

### Requirement: Background sessions
The system SHALL provide `/background <prompt>` (alias `/bg`) on CLI, TUI and messaging surfaces, which starts a separate root session (no lineage parent; same source surface, profile, model and cwd as the current session) running `<prompt>` without blocking the current session, prints `background session <id> started`, and on completion delivers a notification with the session title and final answer (truncated to 2000 characters with a `prometheus sessions show <id>` pointer) to the originating chat or terminal, limited to `sessions.max_background` concurrent background sessions (default 3; exceeding replies `too many background sessions`).
**Milestone:** M2

#### Scenario: Result delivered back
- **GIVEN** a Telegram chat
- **WHEN** the user sends `/background summarise the repo` and keeps chatting
- **THEN** the foreground session SHALL keep answering and the chat SHALL later receive the background session's final answer

### Requirement: External editor composition
The REPL SHALL open `$VISUAL`, then `$EDITOR`, then a platform default (`notepad` on Windows, `vi` elsewhere) on Ctrl-X Ctrl-E with the current input buffer, and through `/prompt [initial text]` (aliases `/compose`, `/edit`) on a temporary Markdown file, submitting the saved content as the next user message, cancelling with `empty prompt; nothing sent` when the saved file is empty or unchanged from an empty start, and deleting the temporary file afterwards.
**Milestone:** M1

#### Scenario: Empty file cancels
- **WHEN** the user types `/prompt` and closes the editor without writing anything
- **THEN** nothing SHALL be sent and the REPL SHALL print `empty prompt; nothing sent`

### Requirement: Windows console behaviour
The CLI SHALL on Windows switch the console to UTF-8 (input and output code page 65001) for its lifetime and restore the previous code pages on exit, enable virtual terminal processing, accept Ctrl+Enter as a newline in addition to Alt+Enter, and report through `prometheus doctor` the shell the `terminal` tool will use (selection owned by `execution-environments`) and whether the install directory is on `PATH`.
**Milestone:** M2

#### Scenario: Code page restored
- **GIVEN** a console with code page 437
- **WHEN** the user runs and exits `prometheus`
- **THEN** the console code page SHALL be 437 again

### Requirement: Local transcript and session commands
The system SHALL provide `/history [N]` printing the last N (default all) active user and assistant messages of the current session locally with `[HH:MM]` prefixes when timestamps are on, `/status` printing model, provider, profile, session id, title, cwd, started and last-activity times, token and cost totals, running state and a recap (turn count, top 5 tools by calls, files touched, latest prompt and reply truncated to 200 characters), `/version` printing the same identity line as `prometheus --version` on every surface, and `/sessions [all] [search <query>]` listing the 10 most recent sessions of the current surface scope (the current chat on messaging, the workspace on CLI) with a `(current)` marker, where `all` lists every session of the profile and on messaging requires an admin user; none of these SHALL call a model.
**Milestone:** M1

#### Scenario: Status without model call
- **WHEN** the user types `/status`
- **THEN** the system SHALL print the session details and recap and no provider request SHALL be made

#### Scenario: All sessions refused for non-admin
- **GIVEN** a messaging user who is not an admin
- **WHEN** they send `/sessions all`
- **THEN** the system SHALL reply that `/sessions all` requires an admin and list nothing

### Requirement: Retry semantics
The `/retry` command SHALL soft-delete (per `session-storage` › Rewind is a soft delete) every message after the last user message and resubmit that user message unchanged with its attachments, SHALL be `busy_policy = interrupt_then_dispatch`, and SHALL print `nothing to retry` when the session has no user message.
**Milestone:** M1

#### Scenario: Retry replaces the answer
- **GIVEN** a session whose last exchange is user `U` and assistant `A`
- **WHEN** the user types `/retry`
- **THEN** `A` SHALL be marked inactive and a new assistant response to `U` SHALL be generated

### Requirement: Queue management commands
The system SHALL provide `/queue <prompt>` (alias `/q`) enqueueing a prompt for after the current turn on every surface including the CLI, and `/queue list`, `/queue edit <n> <text>`, `/queue rm <n>`, `/queue move <a> <b>`, `/queue clear` and `/queue add <text>` (enqueue even when idle, starting it immediately) operating on the session's FIFO queue (owned by `messaging-gateway` › Queued turns run in FIFO order), with 1-based indexes and `no queued item <n>` for an out-of-range index.
**Milestone:** M1

#### Scenario: Reorder queue
- **GIVEN** queued prompts `a`, `b`, `c`
- **WHEN** the user types `/queue move 3 1`
- **THEN** `/queue list` SHALL show `c`, `a`, `b`

### Requirement: Planning-only turn
The system SHALL provide `/plan [task]`, which runs one turn whose effective tool set excludes every tool with `effect = workspace` or `effect = external` except a write limited to `<workspace>/.prometheus/plans/<slug>.md` (slug from the task, deduplicated with `-2`, `-3`), uses the conversation so far as the task when `task` is empty, prints the plan path at the end, and changes no tool definitions sent to the provider (the restriction is enforced by the dispatcher, refusing calls with `not allowed in /plan`).
**Milestone:** M2

#### Scenario: Write outside plans refused
- **WHEN** during `/plan add caching` the model calls `write_file` on `src/main.rs`
- **THEN** the call SHALL be refused with `not allowed in /plan` and only `.prometheus/plans/add-caching.md` SHALL be written

### Requirement: Exit with delete and profile archive slash forms
The `/exit` command SHALL accept `--delete`, which after a destructive confirmation (skipped by `--yes`) ends the session and permanently deletes it per `session-storage` › Deletion semantics, and the registry SHALL provide `/export [profile] [-o <file>]` and `/import <archive> [--name <name>]` on `cli`, `tui` and `desktop` surfaces mapping to `prometheus profile export` and `profile import` with the same defaults and exclusions.
**Milestone:** M2

#### Scenario: Exit and delete
- **WHEN** the user types `/exit --delete --yes`
- **THEN** the REPL SHALL exit 0 and `prometheus sessions show <id>` SHALL exit 66

### Requirement: Onboarding state
The system SHALL store first-run onboarding progress (offered setup, shown one-time tips by id, completed tour steps) in the profile's state store rather than `config.toml`, SHALL show each one-time tip at most once per profile, and SHALL provide `prometheus setup --reset-onboarding` to clear it.
**Milestone:** M1

#### Scenario: Tip shown once
- **GIVEN** the tip `slash-help` was shown in a previous session
- **WHEN** a new session starts in the same profile
- **THEN** the tip SHALL NOT be shown again

### Requirement: Tool configurator
The system SHALL provide `prometheus tools configure [--surface <surface>]`, an interactive editor that enables or disables toolsets per surface (`cli`, `tui`, `gateway.<platform>`, `cron`, `acp`) and selects the provider per tool category (web search, web extract, image generation, TTS, STT, browser, video, X search) from the registered providers, plus the non-interactive forms `prometheus tools enable|disable <toolset> [--surface <s>]` and `prometheus tools provider <category> <provider>`, writing only through the config writer and refusing unknown toolsets or providers with exit 78 listing valid names; changes apply from the next session.
**Milestone:** M1

#### Scenario: Unknown provider refused
- **WHEN** the user runs `prometheus tools provider web_search nosuch`
- **THEN** the command SHALL exit 78 listing the registered web search providers and SHALL NOT write config
