# Execution Environments

## Purpose
Execution environments run model-authored shell commands, background processes, file operations and browser automation on a selected backend: the OS-sandboxed local host, a hardened rootless container, SSH, or remote sandboxes supplied as `terminal-backend` plugins. The key decision is that every backend is a typed registry entry with a declared isolation descriptor, every command and file operation for a session goes through that session's backend under the capability set from `security-boundaries`, and hardening is never weakened silently. In this spec `<profile>` denotes the profile's home directory resolved from the session's ProfileScope.

## ADDED Requirements

### Requirement: Typed backend registry and selection
The system SHALL select the terminal backend from `terminal.backend` (default `local`) among the built-ins `local`, `container` and `ssh` and the backends registered by enabled `terminal-backend` plugins; an unknown name SHALL fail config validation with exit code 78 and `unknown terminal backend '<name>'; available: <list>`, and SHALL never fall back to another backend. In M1 the names `container` and `ssh` SHALL already be reserved registry entries whose selection fails with exit code 69 and `terminal backend '<name>' is available in M2` (their implementation requirements are M2).
**Milestone:** M1

#### Scenario: Unknown backend
- **GIVEN** `terminal.backend = "acmebox"` and no plugin providing `acmebox`
- **WHEN** `prometheus chat` starts
- **THEN** the system SHALL exit with code 78 and list `local`, `container`, `ssh` and every plugin backend

#### Scenario: Reserved backend selected before it ships
- **GIVEN** an M1 build and `terminal.backend = "container"`
- **WHEN** `prometheus chat` starts
- **THEN** the system SHALL exit with code 69 and print `terminal backend 'container' is available in M2`

### Requirement: Built-in backend names are reserved
The system MUST reject a plugin backend registration whose lower-cased name is `local`, `container`, `docker`, `podman` or `ssh` with `terminal backend name '<name>' is reserved`, and SHALL keep the built-in backend.
**Milestone:** M2

#### Scenario: Plugin shadows container
- **WHEN** a plugin registers a backend named `Docker`
- **THEN** the plugin's backend SHALL be rejected and the built-in container backend SHALL remain selectable

### Requirement: Backend isolation descriptor
Every backend SHALL declare a typed descriptor with `isolation` (`os_sandbox`, `container`, `remote`), `uses_host_filesystem` (bool), `default_cwd` (absolute path or `~`), `session_isolated_when_ephemeral` (bool) and `persistent` (bool); a plugin descriptor that is missing a field or holds an invalid value SHALL cause the plugin backend to be rejected at load time, and approval and file-tool decisions SHALL read only this descriptor, never the backend name.
**Milestone:** M2

#### Scenario: Relative default cwd
- **GIVEN** a plugin backend declaring `default_cwd = "work"`
- **WHEN** the plugin loads
- **THEN** the backend SHALL be rejected with `default_cwd must be absolute or '~'`

### Requirement: Remote sandboxes are plugins
Remote execution providers (such as Modal, Daytona, E2B or Vercel-style sandboxes) SHALL be implemented as `terminal-backend` WASM plugins that receive only the network hosts and secrets granted in their manifest, and the host SHALL forward `execute`, `upload`, `download`, `cancel` and `teardown` calls to them; a plugin backend's `execute` result SHALL carry `exit_code`, `stdout`, `stderr` and `cwd_observed` as typed fields so a missing exit code is a contract error, not exit 0.
**Milestone:** M2

#### Scenario: Plugin omits exit code
- **GIVEN** a backend plugin whose `execute` result has no `exit_code`
- **WHEN** a command completes
- **THEN** the result SHALL have `status = "error"` naming the contract violation

### Requirement: Backend readiness checks
The system SHALL check the selected backend's requirements (container runtime reachable and rootless, SSH `terminal.ssh.host` and `terminal.ssh.user` set, plugin `check` call) as the `terminal` tool's availability probe, SHALL expose the failure reason through `prometheus doctor terminal`, and `prometheus doctor terminal` SHALL exit 0 when the backend is ready and 1 otherwise.
**Milestone:** M1

#### Scenario: SSH misconfigured
- **GIVEN** `terminal.backend = "ssh"` without `terminal.ssh.user`
- **WHEN** `prometheus doctor terminal` runs
- **THEN** it SHALL print `ssh backend requires terminal.ssh.host and terminal.ssh.user` and exit 1

### Requirement: Terminal configuration defaults
The system SHALL resolve terminal settings only from the session's ProfileScope config with these defaults: `terminal.timeout_secs` 180, `terminal.max_foreground_secs` 600, `terminal.idle_lifetime_secs` 300, `terminal.persistent_shell` true, `terminal.container.cpus` 1, `terminal.container.memory_mb` 5120, `terminal.container.disk_mb` 51200, `terminal.container.persistent` true, `terminal.container.shm_size` `"1g"`, `terminal.container.pids_limit` 2048, `terminal.ssh.port` 22; settings for an unselected backend SHALL be validated for type only and SHALL NOT prevent the selected backend from working; no terminal setting SHALL be read from environment variables.
**Milestone:** M1

#### Scenario: Two profiles, two backends
- **GIVEN** a gateway serving profile `a` with `container` and profile `b` with `local`
- **WHEN** a turn for `b` runs `terminal`
- **THEN** the command SHALL run on `b`'s local backend

### Requirement: Local backend runs under the OS sandbox
The `local` backend SHALL run every command, background process and code-execution child inside the OS sandbox profile derived from the session's effective capability set as defined by `security-boundaries`, SHALL report the enforcement level (`full`, `partial:<missing features>`, `none`) in `prometheus doctor terminal` and the session banner, and SHALL refuse to execute with `sandbox unavailable: <reason>` when enforcement is below `full` unless the missing features are listed in `security.sandbox.allow_degraded`.
**Milestone:** M1

#### Scenario: Kernel without Landlock
- **GIVEN** a Linux kernel without Landlock and an empty `security.sandbox.allow_degraded`
- **WHEN** the model runs `ls`
- **THEN** the command SHALL NOT run and the result SHALL contain `sandbox unavailable: landlock not supported by kernel`

#### Scenario: Write outside workspace
- **GIVEN** a session whose capability set grants `fs.write` only to the workspace
- **WHEN** the model runs `touch ~/.bashrc`
- **THEN** the command SHALL fail with a permission error from the sandbox and `~/.bashrc` SHALL be unchanged

### Requirement: Container hardening is never silently dropped
The `container` backend SHALL use a rootless Podman or Docker runtime and start every container with `--cap-drop ALL` plus only `CHOWN`, `DAC_OVERRIDE` and `FOWNER`, `--security-opt no-new-privileges`, a read-only root filesystem, tmpfs `/tmp` (`rw,nosuid,size=512m`) and `/var/tmp` (`rw,noexec,nosuid,size=256m`), the configured pids, CPU and memory limits, and labels `prometheus.profile=<profile id>` and `prometheus.session=<session id>`; if the runtime rejects any of these flags the backend SHALL fail with an error naming the flag unless that flag is listed in `security.sandbox.allow_degraded`, in which case it SHALL log at WARN on every start.
**Milestone:** M2

#### Scenario: Runtime rejects no-new-privileges
- **GIVEN** a container runtime that rejects `no-new-privileges` and no matching `allow_degraded` entry
- **WHEN** a container is created
- **THEN** creation SHALL fail with `container hardening 'no_new_privileges' unsupported by runtime` and no container SHALL start

### Requirement: Container lifecycle and orphan reaping
The system SHALL give each session its own container when `terminal.container.persistent = false`, SHALL tear down environments idle longer than `terminal.idle_lifetime_secs` except while a foreground call is in flight or a tracked background process is running, SHALL stop rather than remove persistent containers, and once per gateway start SHALL remove only exited containers labelled with the current profile that are older than twice the idle lifetime.
**Milestone:** M2

#### Scenario: Long foreground command
- **GIVEN** a foreground command running longer than the idle lifetime
- **WHEN** the idle reaper runs
- **THEN** the environment SHALL NOT be torn down

#### Scenario: Other profile's container
- **WHEN** the orphan reaper runs for profile `a`
- **THEN** it SHALL NOT remove containers labelled `prometheus.profile=b`

### Requirement: Working directory resolution
The system SHALL start a session in the configured `terminal.cwd` for `local` (default: the directory `prometheus` was launched from, or the session's workspace for gateway sessions), `~` for `ssh`, `/workspace` for `container` when the workspace is mounted and the descriptor's `default_cwd` otherwise; a host-looking path configured for a non-host backend SHALL be replaced by the backend default with a one-time notice.
**Milestone:** M1

#### Scenario: Host path on a remote backend
- **GIVEN** a remote plugin backend and `terminal.cwd = "/Users/me/project"`
- **WHEN** the session starts
- **THEN** the backend's `default_cwd` SHALL be used and a notice SHALL name the ignored path

### Requirement: Session cwd tracking
The system SHALL update a session's cwd to the directory a foreground command finished in only when the backend reports `cwd_observed`, SHALL NOT let a per-call `workdir` change the session cwd, and SHALL include `cwd` in the result only when it differs from the command's starting directory.
**Milestone:** M1

#### Scenario: workdir does not move the session
- **GIVEN** a session cwd of `/repo`
- **WHEN** the model runs `terminal(command="ls", workdir="/srv")` and then `terminal(command="pwd")`
- **THEN** the second command SHALL print `/repo`

### Requirement: Terminal argument validation
The `terminal` tool SHALL reject `notify_on_complete`, `watch_patterns` or a positive `heartbeat` on a foreground call, `pty = true` without `background = true` (`pty requires background=true`), a non-positive `timeout` (`timeout must be a positive number of seconds (got <n>)`) and a negative `heartbeat`, each before any execution.
**Milestone:** M1

#### Scenario: PTY in foreground
- **WHEN** the model calls `terminal(command="python", pty=true)`
- **THEN** the result SHALL be an error beginning `pty requires background=true` and nothing SHALL run

### Requirement: Foreground cap, promotion and server guard
The system SHALL run a foreground call whose timeout exceeds `terminal.max_foreground_secs` as a tracked background process with completion notification and a `promoted_from_foreground` note, SHALL run a lone trailing `&` as `background = true`, and SHALL refuse a foreground command that uses `nohup`, `disown`, `setsid`, inline `&`, or matches a long-lived server pattern (`npm|pnpm|yarn|bun (run) dev|start|serve|watch`, `docker compose up`, `next dev`, `vite`, `nodemon`, `uvicorn`, `gunicorn`, `cargo watch`, `python -m http.server`) with guidance to resend with `background = true`, except for `--help`, `-h`, `--version` and `-V` invocations.
**Milestone:** M1

#### Scenario: Dev server in foreground
- **WHEN** the model runs `terminal(command="npm run dev")`
- **THEN** the result SHALL begin `This foreground command appears to start a long-lived server/watch process.`

#### Scenario: Long test run
- **WHEN** the model runs `terminal(command="cargo test", timeout=1800)`
- **THEN** a tracked background process SHALL start and the result SHALL contain `promoted_from_foreground`

### Requirement: Pre-execution guards fail closed
The system SHALL complete the guard chain (capability check, approval gate, workdir validation) before any execution under a deadline of `max(timeout, 30)` seconds; if the chain errors or misses the deadline the command MUST NOT run and the result SHALL be `status = "error"` with `pre-execution guard did not finish within <n>s; the command was not run`.
**Milestone:** M1

#### Scenario: Wedged guard
- **GIVEN** an approval transport that never answers within the deadline
- **WHEN** a command is submitted
- **THEN** the command SHALL NOT execute

### Requirement: Foreground result shaping
The foreground result SHALL contain `output`, `exit_code` and `status`, plus `cwd`, `exit_code_meaning`, `hint`, `environment_recreated` and spill fields when applicable; output SHALL pass through `transform_terminal_output` hooks, ANSI stripping, secret redaction, and head/tail truncation at `tool_output.max_chars` (default 50,000) keeping 40% head and 60% tail around `... [OUTPUT TRUNCATED - <n> chars omitted out of <total> total] ...`.
**Milestone:** M1

#### Scenario: Oversized output
- **GIVEN** a command printing 120,000 characters
- **WHEN** the result is returned
- **THEN** it SHALL keep the first 20,000 and last 30,000 characters around one truncation marker

### Requirement: Spilled full output is redacted and private
When foreground output exceeds the capture window the system SHALL write the full output, ANSI-stripped and secret-redacted, to `<profile>/cache/terminal-spill/<session_id>/` with mode 0600 and exclusive create before exposing `full_output_path`, SHALL drop the path and delete the file if redaction fails, and SHALL prune spill files after 7 days.
**Milestone:** M1

#### Scenario: Redaction fails
- **GIVEN** the spill file cannot be re-read for redaction
- **WHEN** the result is finalized
- **THEN** the result SHALL contain no `full_output_path` and the file SHALL not exist

### Requirement: Infrastructure failures are degraded results
The system SHALL turn a backend connection failure (container runtime down, SSH host unreachable, remote sandbox API error) into `{"output": "", "exit_code": -1, "status": "degraded", "reason": ..., "retry_hint": ...}` and evict the cached environment so the next call recreates it, unless `terminal.degraded_mode = "fail"`, which returns `status = "error"`; `terminal.degraded_mode` SHALL accept only `degrade` (default) or `fail`.
**Milestone:** M1

#### Scenario: Container runtime down
- **GIVEN** the container runtime is not running
- **WHEN** the model runs a command
- **THEN** the result SHALL have `status = "degraded"` and the next call SHALL attempt a fresh environment

### Requirement: Child environments are built from an allowlist
Every child process (terminal command, background process, code execution, MCP stdio server, Python bridge, hook script) SHALL receive an environment built from a fixed allowlist (`PATH`, `HOME`, `USER`, `LANG`, `LC_*`, `TERM`, `SHELL`, `TMPDIR`, `TZ`, platform location variables) plus only those secrets the session holds a `secrets.read:<NAME>` grant for and the target declares, resolved from the session's ProfileScope; the child environment MUST NOT be derived by copying the parent process environment.
**Milestone:** M1

#### Scenario: Bot token never reaches a shell
- **GIVEN** the gateway holds `TELEGRAM_BOT_TOKEN`
- **WHEN** the model runs `terminal(command="env")`
- **THEN** the output SHALL NOT contain `TELEGRAM_BOT_TOKEN`

#### Scenario: Declared skill secret
- **GIVEN** an enabled skill declaring `OPENAI_API_KEY` and a `secrets.read:OPENAI_API_KEY` grant
- **WHEN** the skill's script runs
- **THEN** `OPENAI_API_KEY` SHALL be present and no other provider key SHALL be present

### Requirement: Background process registry
The `process` tool SHALL support actions `list`, `poll`, `log`, `wait`, `kill`, `write`, `submit`, `close` and `handoff`, SHALL require `process_id` for every action except `list` (`process_id is required for <action>`), SHALL accept a unique id prefix, SHALL track at most 64 processes per session pruning the oldest finished first, SHALL keep a 200,000-character rolling output buffer per process, and SHALL expire a finished process 1,800 seconds after it finished (not after it started); an unknown action SHALL return `unknown process action: <a>; use one of list, poll, log, wait, kill, write, submit, close, handoff`.
**Milestone:** M1

#### Scenario: Long job is not pruned on exit
- **GIVEN** a process that ran for 2 hours and just exited
- **WHEN** the model polls it 10 minutes later
- **THEN** its final output and exit code SHALL still be available

### Requirement: Background notifications are throttled
The system SHALL deliver `watch_patterns` matches at most once per 15 seconds per process, SHALL disable pattern watching and fall back to completion notification after 8 deliveries or 3 consecutive throttled windows, SHALL cap notifications globally at 15 per 10 seconds per session with a 30-second cooldown, and SHALL clamp `heartbeat` to at least 60 seconds carrying only output produced since the previous heartbeat.
**Milestone:** M2

#### Scenario: Ninth match
- **GIVEN** a process whose pattern has matched 8 times
- **WHEN** it matches again
- **THEN** the system SHALL disable watching and notify only on completion

### Requirement: Subagent process handoff
The `handoff` action SHALL be available only to subagent sessions, SHALL transfer a running process the child owns to its parent session, and SHALL refuse beyond 3 handoffs per subagent with `handoff cap reached (3 per subagent); wait on or kill the rest yourself.`
**Milestone:** M2

#### Scenario: Fourth handoff
- **GIVEN** a subagent that has handed off 3 processes
- **WHEN** it hands off another
- **THEN** the system SHALL return the handoff-cap error and the process SHALL stay with the subagent

### Requirement: File tools route through the backend
`read_file`, `write_file`, `patch`, `search_files` and `list_dir` SHALL operate through the session's backend and SHALL touch the host filesystem directly only when the backend descriptor declares `uses_host_filesystem = true`, in which case host access SHALL be checked against the session's `fs.read`/`fs.write` capabilities before the operation.
**Milestone:** M1

#### Scenario: Container backend
- **GIVEN** a container backend without a workspace mount
- **WHEN** the model reads `/etc/hostname`
- **THEN** the container's file SHALL be returned, not the host's

### Requirement: read_file guards, pagination and dedup
`read_file` SHALL default to offset 1 and limit 2,000 lines, cap formatted content at `files.read_max_chars` (default 100,000) returning `next_offset`, refuse device files (`Cannot read '<path>': this is a device file that would block or produce infinite output.`), FIFOs, sockets, `/proc/<pid>/environ|mem|maps`-style secret-leaking files and binary files (`Cannot read binary file '<path>'; use vision_analyze for images or terminal for binary inspection`), and SHALL answer an identical read (same path, range, unchanged content, same compaction generation) with an `{"status": "unchanged"}` stub the first time and a `BLOCKED:` refusal on further repeats.
**Milestone:** M1

#### Scenario: Third identical read
- **GIVEN** a region read twice already and unchanged
- **WHEN** the model reads it again
- **THEN** the result SHALL begin `BLOCKED: You have already read this exact region`

### Requirement: Write guards
`write_file` and `patch` SHALL refuse writes to `/etc/`, `/boot/`, `/usr/lib/systemd/`, `/private/etc/`, `/private/var/db/`, container runtime sockets, and every profile's `config.toml`, `secrets.toml` and `grants.toml` under `PROMETHEUS_HOME` (resolved through the ProfileScope, never a hardcoded path); SHALL require human approval in every approval mode, failing closed without a channel, for files named `AGENTS.md`, `CLAUDE.md`, `SOUL.md`, `IDENTITY.md` or `.cursorrules` (case-insensitive); and SHALL refuse a plain-text overwrite of a binary file.
**Milestone:** M1

#### Scenario: Self-edit of grants
- **WHEN** the model calls `write_file` on the active profile's `grants.toml`
- **THEN** the system SHALL refuse with `refusing to write Prometheus control file: grants.toml` and the file SHALL be unchanged

#### Scenario: Instruction file under yolo
- **GIVEN** `--yolo`
- **WHEN** the model writes `AGENTS.md`
- **THEN** the system SHALL still request human approval

### Requirement: Stale overwrite protection
`write_file` SHALL refuse, before any mutation, to overwrite an existing file that this session has not fully read or that changed on disk since it was last read or written by the session, returning `status = "error"` with `stale_write_blocked: true` and the path; new files SHALL be written without a prior read.
**Milestone:** M1

#### Scenario: External edit
- **GIVEN** the session read `a.rs` and another process modified it
- **WHEN** the model writes `a.rs`
- **THEN** the write SHALL be refused and `a.rs` SHALL keep the external edit

### Requirement: File snapshots and undo (upgrade)
Before any `write_file`, `patch`, delete or rename performed by a file tool, the system SHALL store the file's prior content (or its absence) in the content-addressed store with a record keyed by session, turn and call id; `/undo [n]` (default 1 turn), `prometheus files undo --session <id> [--turn <n>] [--dry-run]` and the RPC method `files.undo` SHALL restore every file changed in the last `n` turns, SHALL refuse a file whose current content differs from what the session wrote (`conflict: <path> changed since turn <n>; use --force to overwrite`) unless `--force` is given, and SHALL append (never rewrite) a note to the conversation recording the undo; snapshots SHALL be pruned after `files.snapshot.retention_secs` (default 604800) or when the profile's snapshot store exceeds `files.snapshot.max_mb` (default 1024), oldest first.
**Milestone:** M2

#### Scenario: Undo last turn
- **GIVEN** a turn in which the model patched `src/lib.rs` and created `src/new.rs`
- **WHEN** the user runs `/undo`
- **THEN** `src/lib.rs` SHALL have its previous content and `src/new.rs` SHALL no longer exist

#### Scenario: Conflicting edit
- **GIVEN** the user edited `src/lib.rs` after the model's patch
- **WHEN** the user runs `/undo` without `--force`
- **THEN** `src/lib.rs` SHALL be left unchanged and the conflict SHALL be reported

### Requirement: Terminal workspace checkpoints (upgrade)
When `files.snapshot.terminal = "workspace"` (default `off`), the system SHALL record a manifest of the session workspace (path, size, mtime, content hash, at most `files.snapshot.max_workspace_mb`, default 512) before each `exec`-class terminal command and store changed files' prior contents afterwards, so that `/undo` also reverts file changes made by terminal commands inside the workspace; a workspace over the limit SHALL skip checkpointing with a one-time notice rather than fail the command.
**Milestone:** M3

#### Scenario: Undo a sed edit
- **GIVEN** `files.snapshot.terminal = "workspace"`
- **WHEN** the model runs `sed -i s/a/b/ notes.txt` and the user runs `/undo`
- **THEN** `notes.txt` SHALL have its prior content

### Requirement: Browser session resolution
The system SHALL reuse a live session for the same session key and otherwise create one in this order: an explicit CDP endpoint (`/browser connect <url>` then `browser.cdp_url`); a configured cloud browser provider plugin (`browser.provider`, default `local`; unknown names fail config validation with exit code 78); a local Chromium from the managed tool store launched with a throwaway profile and `--remote-debugging-port=0`; a cloud provider failure SHALL fall back to a local session marked `fallback_from_cloud`, and failure of both SHALL return `Cloud provider <name> failed (<reason>) and local fallback also failed (<reason>)`.
**Milestone:** M1

#### Scenario: Cloud quota exhausted
- **GIVEN** `browser.provider = "browserbase"` whose session creation fails
- **WHEN** `browser_navigate` needs a session
- **THEN** the system SHALL use a local session flagged `fallback_from_cloud: true`

### Requirement: Local browser isolation
A locally launched browser SHALL run under the OS sandbox with write access only to its throwaway profile directory under `<profile>/cache/browser/` and network subject to the session's egress policy, and SHALL use the user's real browser profile only after explicit consent recorded as the grant `browser.real_profile` in `grants.toml`.
**Milestone:** M2

#### Scenario: No consent
- **GIVEN** no `browser.real_profile` grant
- **WHEN** a browser session is created
- **THEN** the system SHALL launch with a throwaway profile

### Requirement: Browser URL safety
Before navigation the system SHALL refuse URLs that embed a secret-shaped value or a known secret value (raw or URL-decoded) with `Blocked: URL contains what appears to be an API key or token.`, SHALL always refuse cloud metadata endpoints with `Blocked: URL targets a cloud metadata endpoint`, and SHALL refuse private or internal addresses with `Blocked: URL targets a private or internal address` unless the session's egress policy allows that address.
**Milestone:** M1

#### Scenario: Metadata endpoint locally
- **GIVEN** a local browser and private addresses allowed by egress policy
- **WHEN** the model navigates to `http://169.254.169.254/latest/meta-data`
- **THEN** the navigation SHALL be refused

### Requirement: Browser supervision, dialogs and cleanup
The system SHALL supervise each browser session over CDP, replacing a supervisor bound to a different endpoint and giving up after 5 consecutive reconnect failures; `browser.dialog_policy` SHALL be `must_respond` (default), `auto_dismiss` or `auto_accept` with unanswered dialogs dismissed after `browser.dialog_timeout_secs` (default 300); sessions idle longer than `browser.inactivity_timeout_secs` (default 120, read per ProfileScope at use time) SHALL be closed; every launched browser SHALL be recorded in a 0700 directory under `<profile>/cache/browser/records/` so a later process can reap a crashed owner's browser after verifying pid, start time and profile directory.
**Milestone:** M1

#### Scenario: Crashed browser
- **GIVEN** a cached local session whose browser process died
- **WHEN** the next browser call arrives
- **THEN** the dead record SHALL be removed and a fresh browser launched
