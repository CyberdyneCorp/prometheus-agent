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
The system SHALL resolve terminal settings only from the session's ProfileScope config with these defaults: `terminal.timeout_secs` 180, `terminal.max_foreground_secs` 600, `terminal.idle_lifetime_secs` 300, `terminal.persistent_shell` true, `terminal.container.cpus` 1, `terminal.container.memory_bytes` 5368709120, `terminal.container.disk_bytes` 53687091200, `terminal.container.persistent` true, `terminal.container.shm_size` `"1g"`, `terminal.container.pids_limit` 2048, `terminal.ssh.port` 22; settings for an unselected backend SHALL be validated for type only and SHALL NOT prevent the selected backend from working; no terminal setting SHALL be read from environment variables.
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
Before any `write_file`, `patch`, delete or rename performed by a file tool, the system SHALL store the file's prior content (or its absence) in the content-addressed store with a record keyed by session, turn and call id; `/undo [n]` (default 1 turn), `prometheus files undo --session <id> [--turn <n>] [--dry-run]` and the RPC method `files.undo` SHALL restore every file changed in the last `n` turns, SHALL refuse a file whose current content differs from what the session wrote (`conflict: <path> changed since turn <n>; use --force to overwrite`) unless `--force` is given, and SHALL append (never rewrite) a note to the conversation recording the undo; snapshots SHALL be pruned after `files.snapshot.retention_secs` (default 604800) or when the profile's snapshot store exceeds `files.snapshot.max_bytes` (default 1073741824), oldest first.
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
When `files.snapshot.terminal = "workspace"` (default `off`), the system SHALL record a manifest of the session workspace (path, size, mtime, content hash, at most `files.snapshot.max_workspace_bytes`, default 536870912) before each `exec`-class terminal command and store changed files' prior contents afterwards, so that `/undo` also reverts file changes made by terminal commands inside the workspace; a workspace over the limit SHALL skip checkpointing with a one-time notice rather than fail the command.
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
- **WHEN** `browser_exec` needs a session
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

### Requirement: read_file document extraction
`read_file` SHALL convert documents to paginated Markdown instead of refusing them as binary: `.ipynb`, `.docx`, `.xlsx` and SQLite files (`.db`, `.sqlite`, `.sqlite3`) with a built-in converter, and `.pdf`, legacy Office (`.doc`, `.xls`, `.ppt`, `.pptx`), OpenDocument (`.odt`, `.ods`, `.odp`), `.rtf` and `.epub` with a managed `doc-converter` runtime installed under the `runtimes.allow_lazy_install` policy (when it is unavailable those formats SHALL keep the binary-file refusal plus `install the doc-converter runtime to read <ext> files`); SQLite files SHALL be opened read-only and immutable and rendered as each table's `CREATE` statement, row count and first 5 rows plus the lists of indexes, views and triggers; notebook cell outputs longer than 20,000 characters SHALL be truncated with a marker naming a command that extracts the full output; a PDF in which more than 20% of pages, or at least 10 pages, yield no text SHALL be prefixed with `[EXTRACTION COVERAGE WARNING: <n> of <total> pages in this PDF yielded no text ...]` listing each empty page range labelled with the last text extracted before it; files inside a non-host backend SHALL be downloaded through the backend and converted on the host.
**Milestone:** M2

#### Scenario: SQLite schema overview
- **GIVEN** a live `app.db` with tables `users` and `orders`
- **WHEN** the model calls `read_file(path="app.db")`
- **THEN** the result SHALL contain both `CREATE TABLE` statements, row counts and at most 5 rows per table, and `app.db` SHALL NOT be modified

#### Scenario: Scanned PDF
- **GIVEN** a 311-page PDF where pages 42-77 have no text layer
- **WHEN** the model reads it
- **THEN** the result SHALL begin with the coverage warning listing `pages 42-77 (36 pages)` and the text preceding page 42

### Requirement: read_file encoding, conflict markers and path suggestions
`read_file` SHALL decode a file starting with a UTF-16 byte-order mark, or detected as UTF-16 by a NUL-byte-pattern heuristic, as text instead of refusing it as binary; SHALL add `conflict_blocks: <n>` and the hint `resolve the merge conflicts before editing around them` when the returned range contains `n` balanced `<<<<<<< `/`>>>>>>> ` marker pairs at line start; and on a missing path SHALL return `File not found: <path>` followed by up to 5 `Did you mean` suggestions of existing paths in the same or parent directory ranked by name similarity.
**Milestone:** M1

#### Scenario: UTF-16 log file
- **GIVEN** a Windows-generated `install.log` encoded as UTF-16LE with a BOM
- **WHEN** the model reads it
- **THEN** the result SHALL contain the decoded text, not the binary-file refusal

#### Scenario: Typo in path
- **GIVEN** `src/config.rs` exists
- **WHEN** the model reads `src/conifg.rs`
- **THEN** the result SHALL suggest `src/config.rs`

### Requirement: Protected secret files and device-namespace paths
File tools SHALL refuse to read (but SHALL allow writing and creating) project-local `.env`, `.env.*` (except `.env.example`, `.env.sample` and `.env.template`) and `.envrc` files with `Refusing to read '<path>': it typically holds secrets; ask the user to share the specific value you need`; SHALL require approval for writes to `~/.ssh/config`; and SHALL reject, for reads and writes on every platform, paths using the Windows NT or device namespace (`\??\`, `\\.\`, `\\?\UNC\`, `\\?\GLOBALROOT`) or a UNC host (`\\host\share`) with `Refusing device-namespace or UNC path '<path>'`, so a crafted path cannot trigger outbound authentication.
**Milestone:** M1

#### Scenario: Read project .env
- **WHEN** the model calls `read_file(path=".env.local")`
- **THEN** the call SHALL be refused and no content SHALL be returned

#### Scenario: UNC path
- **WHEN** the model calls `read_file(path="\\\\attacker.example\\share\\x")`
- **THEN** the call SHALL be refused without any network access

### Requirement: patch tool contract
`patch` SHALL accept `mode = "replace"` (default) with `path`, `old_string`, `new_string` and `replace_all` (default false), and `mode = "v4a"` with a `patch` string in the `*** Begin Patch` / `*** End Patch` format supporting `Add File`, `Update File` (with `Move to`) and `Delete File`; in replace mode it SHALL try, in order, exact, line-trimmed, trailing-whitespace-normalized, indentation-flexible, unicode-normalized (quotes, dashes, NBSP), escape-normalized, whitespace-collapsed, block-anchor (first and last line) and context-similarity matching, stopping at the first strategy with any match; more than one match without `replace_all` SHALL fail with `old_string matches <n> locations; add context or set replace_all=true`, no match SHALL fail with the closest candidate region shown; a V4A patch SHALL be applied atomically (all files or none); the result SHALL contain a unified diff of the change, `match_strategy` when not exact, and the post-write diagnostics; handlers SHALL accept both argument shapes regardless of which schema variant was advertised.
**Milestone:** M1

#### Scenario: Ambiguous match
- **GIVEN** a file containing `let x = 1;` on two lines
- **WHEN** the model patches `old_string = "let x = 1;"` without `replace_all`
- **THEN** the patch SHALL fail naming 2 locations and the file SHALL be unchanged

#### Scenario: V4A with one failing hunk
- **GIVEN** a V4A patch updating `a.rs` and `b.rs` where the `b.rs` hunk does not apply
- **WHEN** it is applied
- **THEN** neither file SHALL be modified and the error SHALL name `b.rs`

### Requirement: search_files contract
`search_files` SHALL accept `pattern` (regex), `target` (`content` default, or `files` for glob matching names), `path` (default session cwd), `file_glob`, `limit` (default 50, maximum 500), `offset` (default 0), `order` (`discovery` default, or `modified` newest first), `output_mode` (`content` default, `files_only`, `count`) and `context` (lines, default 0, maximum 10); it SHALL respect `.gitignore` and skip binary files and VCS directories, SHALL return content matches as `<path>:<line>:<text>` with lines truncated to 500 characters, SHALL report `total_matches` and `truncated` with the next `offset` when more remain, SHALL use the built-in regex engine on host-filesystem backends and the backend's `rg` (falling back to `grep -rn`) on other backends, and SHALL reject an invalid regex with `invalid pattern: <reason>` before searching.
**Milestone:** M1

#### Scenario: Count mode
- **WHEN** the model calls `search_files(pattern="TODO", output_mode="count")`
- **THEN** the result SHALL list per-file match counts and no line content

### Requirement: Post-write diagnostics
After every successful `write_file` and `patch` the system SHALL run an in-process syntax check for the file's language (at least Rust, Python, JavaScript/TypeScript, JSON, TOML, YAML) and report it as `lint: {status: ok|error|skipped, output}`, and when the syntax is clean SHALL request semantic diagnostics from `lsp-diagnostics`, reporting only diagnostics introduced by this edit as `lsp_diagnostics`; neither check SHALL ever fail, delay beyond its budget or roll back the write, and any checker failure SHALL degrade to `status = "skipped"`.
**Milestone:** M1

#### Scenario: Syntax error introduced
- **WHEN** the model writes a JSON file with a trailing comma
- **THEN** the file SHALL be written and the result SHALL carry `lint.status = "error"` with the parser message and line

#### Scenario: Checker crashes
- **GIVEN** the YAML checker panics on a file
- **WHEN** the model writes that file
- **THEN** the write SHALL succeed with `lint.status = "skipped"`

### Requirement: Quiet-wait auto-notify
When `terminal.process_wait_auto_notify = true` (default) and the model issues two consecutive `process` `wait` or `poll` calls on a running process that produced no new output in between, the system SHALL enable completion notification for that process and append `No new output; you will be notified when it exits. Do other work or end your turn instead of polling.`; new output, process exit or any other action on that process SHALL reset the count; the switch SHALL NOT apply in contexts that cannot receive a later completion (single-query `chat -z`, cron, kanban workers, stateless API requests, subagents), when background notifications are disabled for the profile, or for a process whose completion is delivered to another session.
**Milestone:** M2

#### Scenario: Second unchanged poll
- **GIVEN** an interactive session and a silent running build
- **WHEN** the model polls it twice with no new output
- **THEN** the second result SHALL announce completion notification and the exit SHALL later arrive as its own message

#### Scenario: Cron job
- **GIVEN** a cron run
- **WHEN** the model polls a silent process twice
- **THEN** plain polling SHALL continue with no notification switch

### Requirement: Retained process receipts
When a tracked process leaves the live registry, the system SHALL keep a receipt (command, exit code, start and end time, last 200,000 characters of redacted output) in `state.db` for the newest 64 receipts per profile for at most 7 days, readable through `process` `poll` and `log` with the original `process_id` only from the owning session's lineage (the session, its resumptions and its compression continuations, matched by `root_session_id`); `process` `list` SHALL include the lineage's receipts marked `retained`, and a request from any other session SHALL get `unknown process id` even with an exact id.
**Milestone:** M2

#### Scenario: Read after compression
- **GIVEN** a process that finished 3 hours ago in a session since compressed into a continuation
- **WHEN** the continuation calls `process(action="log", process_id=<id>)`
- **THEN** the retained output SHALL be returned

#### Scenario: Other conversation
- **WHEN** an unrelated session calls `process(action="poll", process_id=<id>)`
- **THEN** it SHALL get `unknown process id`

### Requirement: Sudo password handling
For a terminal command whose executable, after optional `env` options and assignments, is literally `sudo` (bare, absolute or quoted path) the system SHALL run sudo with `-S` and supply the password from, in order, the session's cached password, the secret `SUDO_PASSWORD` when the session holds `secrets.read:SUDO_PASSWORD`, or a masked `sudo.request` prompt to the session's interactive surface whose answer is cached in memory for the session only; shell payloads (`bash -c '...'`, `env -S`, dynamic paths) SHALL NOT be rewritten; subagents, cron, single-query and surfaces without a secure prompt SHALL NOT prompt, and on failure in a messaging session the result SHALL add the hint to store `SUDO_PASSWORD` with `prometheus secrets set`; the password SHALL never appear in tool results, logs, trajectories or the session store.
**Milestone:** M2

#### Scenario: Interactive sudo
- **GIVEN** an interactive CLI session
- **WHEN** the model runs `sudo apt-get update` twice
- **THEN** the user SHALL be prompted once and the second command SHALL reuse the cached password

#### Scenario: Subagent sudo
- **GIVEN** a subagent without `SUDO_PASSWORD`
- **WHEN** it runs `sudo id`
- **THEN** no prompt SHALL be shown and the result SHALL explain that subagents cannot prompt for sudo

### Requirement: Shell startup files
Terminal commands SHALL run in a non-interactive, non-login shell; when `terminal.auto_source_bashrc = true` (default) and the shell is bash, the persistent shell SHALL source `~/.bashrc` once at shell start, and it SHALL then source each path in `terminal.shell_init_files` (default empty, `~` expanded, missing files skipped with a one-time notice); the whole init step SHALL be bounded by `terminal.shell_init_timeout_secs` (default 10), after which the shell SHALL be restarted without init files and the first result SHALL carry the hint `shell init timed out; guard interactive-only code in your rc file with: case $- in *i*) ;; *) return;; esac`.
**Milestone:** M1

#### Scenario: rc file blocks on read
- **GIVEN** a `~/.bashrc` that calls `read`
- **WHEN** the model runs `git status`
- **THEN** after at most 10 seconds the command SHALL run in a shell without init files and the result SHALL carry the shell-init hint

### Requirement: Non-secret environment passthrough
The system SHALL add to every child environment the variables named in `terminal.env_passthrough` (default empty) with values taken from the launch environment, and for container backends SHALL additionally set the literal `terminal.container.env` map (default empty); a name or value that matches a stored secret name, a registered provider-native secret name or the `PMT_SECRET_*` pattern SHALL be rejected at config validation with `terminal.env_passthrough: '<NAME>' is a secret; grant it with secrets.read instead` (exit 78).
**Milestone:** M1

#### Scenario: Pass a toolchain variable
- **GIVEN** `terminal.env_passthrough = ["JAVA_HOME"]` and `JAVA_HOME` set at launch
- **WHEN** the model runs `echo $JAVA_HOME`
- **THEN** the launch value SHALL be printed

#### Scenario: Secret in passthrough
- **GIVEN** `terminal.env_passthrough = ["OPENAI_API_KEY"]`
- **WHEN** config is validated
- **THEN** validation SHALL fail with exit code 78

### Requirement: Container backend options
The `container` backend SHALL accept `terminal.container.image` (default the published `prometheus-sandbox` image pinned by digest for the running release), `terminal.container.volumes` (`host:container[:ro]` entries; a host path under `PROMETHEUS_HOME` or a credential directory denied by `security-boundaries` SHALL be rejected), `terminal.container.mount_workspace` (default true: the session workspace at `/workspace`), `terminal.container.network` (default true; false runs with no network), `terminal.container.run_as_host_user` (default false: `--user <uid>:<gid>`), `terminal.container.shared_key` (default empty: when set, sessions with the same key in the profile share one container) and `terminal.container.extra_args` (default empty); an extra argument that would weaken hardening (`--privileged`, `--cap-add`, `--security-opt` other than adding restrictions, `--pid=host`, `--network=host`, `--userns=host`, a `-v` of the runtime socket) SHALL be rejected unless covered by `security.sandbox.allow_degraded`.
**Milestone:** M2

#### Scenario: Network off
- **GIVEN** `terminal.container.network = false`
- **WHEN** the model runs `curl https://example.com` in the container
- **THEN** the command SHALL fail to resolve or connect

#### Scenario: Privileged extra arg
- **GIVEN** `terminal.container.extra_args = ["--privileged"]` and no matching `allow_degraded`
- **WHEN** the container is created
- **THEN** creation SHALL fail naming `--privileged`

### Requirement: Remote file sync and sync-back
For backends whose descriptor has `uses_host_filesystem = false`, the system SHALL upload granted host inputs (attached files, skill scripts, `read_file`/`write_file` payloads) on demand, and when `terminal.sync_back = true` (default true for `ssh`, false otherwise) SHALL copy files changed under the remote workspace back to the host workspace at session end and on `/sync`, skipping files larger than `terminal.sync_back_max_bytes` (default 2147483648 in total per sync) with a notice listing what was skipped, and never overwriting a host file modified after the last sync without reporting a conflict.
**Milestone:** M2

#### Scenario: Sync-back cap
- **GIVEN** an SSH session that produced 3 GiB of build output in the workspace
- **WHEN** the session ends
- **THEN** sync-back SHALL stop at the 2 GiB budget and list the skipped files

### Requirement: First-party backend plugins
The project SHALL publish signed first-party `terminal-backend` plugins `singularity` (Singularity/Apptainer, config `terminal.singularity.image` as a SIF path or `docker://` reference, persistent overlay per profile, `--containall`), `modal`, `daytona` and `vercel`; the `modal` plugin SHALL accept `terminal.modal.mode` `direct` (the user's `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` secrets), `managed` (a subscription gateway credential, recorded as `billing_source = subscription:<provider>`) or `auto` (default: `direct` when Modal secrets exist, else `managed` when a subscription is signed in, else unavailable with the reason), and the chosen mode SHALL be shown by `prometheus doctor terminal`.
**Milestone:** M2

#### Scenario: Modal auto without credentials
- **GIVEN** `terminal.backend = "modal"`, `terminal.modal.mode = "auto"`, no Modal secrets and no subscription
- **WHEN** `prometheus doctor terminal` runs
- **THEN** it SHALL report the backend unavailable naming both missing options and exit 1

### Requirement: Windows local shell
On Windows the `local` backend SHALL run commands through the shell chosen by `terminal.windows_shell` (`auto` default, `git-bash`, `pwsh`, `powershell`): `auto` SHALL use Git Bash resolved from `terminal.windows_bash_path`, then the Git for Windows install location, then `bash.exe` on `PATH` excluding the WSL launcher (`System32\bash.exe`) and `WindowsApps` aliases, and otherwise fall back to `pwsh`, then Windows PowerShell, with a one-time notice that POSIX shell syntax is unavailable; an explicitly selected shell that is absent SHALL make the terminal tool unavailable with `<shell> not found` and the install hint; the CLI SHALL switch its console and child consoles to UTF-8 (code page 65001) and restore the original code pages on exit; `prometheus doctor terminal` SHALL report the chosen shell and which POSIX-only features (persistent shell, `pty`, signals) are degraded.
**Milestone:** M1

#### Scenario: Only WSL bash on PATH
- **GIVEN** Windows with no Git for Windows and `C:\Windows\System32\bash.exe` on PATH
- **WHEN** a session starts with `terminal.windows_shell = "auto"`
- **THEN** the backend SHALL use `pwsh` or Windows PowerShell and SHALL NOT use the WSL launcher

### Requirement: browser_exec tool
The browser surface exposed to models SHALL be `browser_exec(code, session?, timeout_secs?, local?)`, which runs model-written Python 3 on the managed CPython runtime under the OS sandbox with helpers pre-imported (`new_tab`, `goto_url`, `wait_for_load`, `page_info`, `js`, `fill_input`, `click_at_xy`, `capture_screenshot`, `cdp`, `ensure_real_tab`) bound to the resolved browser session and returns what the code prints; the browser state (tabs, cookies, page) SHALL persist across calls while interpreter variables SHALL NOT; a per-session workspace directory SHALL persist across calls and be returned as `workspace`; `timeout_secs` SHALL default to 300 and be clamped to 1..1800; a named `session` SHALL get its own driver and browser; screenshots SHALL be attached as images only when the active model accepts image input and otherwise returned as paths; the tool SHALL be offered only in sessions that also have the `terminal` toolset, and `local` SHALL appear only when real-profile browsing is enabled.
**Milestone:** M1

#### Scenario: Batched sub-procedure
- **WHEN** the model calls `browser_exec(code="new_tab('https://example.com'); wait_for_load(); print(page_info())")`
- **THEN** the result SHALL contain the page summary and a later call SHALL see the same open tab

#### Scenario: No terminal toolset
- **GIVEN** a session whose toolsets exclude `terminal`
- **WHEN** the tool array is built
- **THEN** `browser_exec` SHALL NOT be present

### Requirement: browser_cdp and browser_dialog tools
`browser_cdp(method, params?, target_id?, frame_id?, timeout_secs?)` and `browser_dialog(action, prompt_text?, dialog_id?)` SHALL be offered only while an explicit CDP endpoint is configured (`/browser connect` or `browser.cdp_url`); `browser_cdp` SHALL require `target_id` for page-level domains (`Page`, `Runtime`, `DOM`, `Emulation`, `Input`, `Network` per-target), route `frame_id` calls through the supervisor's session for that out-of-process frame, default `timeout_secs` to 30 (max 300), and refuse `Browser.close`, `Browser.crash`, `Target.closeTarget` of a target it did not open, and any URL parameter blocked by browser URL safety; `browser_dialog` SHALL accept `accept` or `dismiss`, pass `prompt_text` to `prompt()` dialogs, target the oldest pending dialog when `dialog_id` is omitted, and return `no pending dialog` when none is open.
**Milestone:** M2

#### Scenario: Dialog answered
- **GIVEN** a connected browser with a pending `confirm()` dialog under `dialog_policy = "must_respond"`
- **WHEN** the model calls `browser_dialog(action="accept")`
- **THEN** the dialog SHALL be accepted and page scripts SHALL resume

#### Scenario: Closing the browser
- **WHEN** the model calls `browser_cdp(method="Browser.close")`
- **THEN** the call SHALL be refused

### Requirement: Local browser options and headed mode
The local browser SHALL honour `browser.executable_path` (default empty: managed `chromium` runtime, then a system Chromium-family binary), `browser.user_data_dir` (default empty: throwaway profile; a relative path resolves under `<profile>`), `browser.chromium_args` (default empty), `browser.launch_timeout_secs` (default 30) and `browser.headed` (default false); with `headed = true` the browser SHALL open a visible window and SHALL NOT be cleaned up between turns (idle reaping still applies); on a host without a display the browser SHALL run headless with a one-time notice; headed mode SHALL not affect cloud sessions, and `chromium_args` that disable the Chromium sandbox or web security SHALL require the matching `security.sandbox.allow_degraded` entry.
**Milestone:** M2

#### Scenario: Headless server
- **GIVEN** `browser.headed = true` on Linux with no display
- **WHEN** a browser session starts
- **THEN** it SHALL run headless and notify once that no display is available

### Requirement: First-party cloud browser providers
The project SHALL ship first-party `browser.provider` plugins `browserbase`, `browser-use` and `firecrawl`, plus a `subscription` provider served by a signed-in provider subscription; `browser.provider` SHALL default to `local` and SHALL never be auto-selected from the presence of provider API keys (setup MAY suggest a provider when it detects one); each provider SHALL declare its secrets and network hosts in its manifest and its sessions SHALL carry `provider` in `prometheus browser status`.
**Milestone:** M2

#### Scenario: Key present, not configured
- **GIVEN** `BROWSERBASE_API_KEY` in the secrets store and no `browser.provider` set
- **WHEN** a browser session is created
- **THEN** a local session SHALL be used

### Requirement: Private URLs route to a local browser
When a cloud browser provider is active and `browser.auto_local_for_private_urls = true` (default), a `browser_exec` call whose code navigates to a private, loopback or LAN destination (per `security-boundaries` private ranges, plus `*.local`, `*.lan`, `*.internal`, `localhost`) SHALL run against a local sidecar browser for that session while public URLs stay on the cloud provider, so the cloud provider never receives the private URL; with the switch off such navigation SHALL be refused with `Blocked: URL targets a private or internal address` unless the egress policy allows the address; cloud metadata endpoints SHALL be refused on every route.
**Milestone:** M2

#### Scenario: Localhost under Browserbase
- **GIVEN** `browser.provider = "browserbase"`
- **WHEN** the model opens `http://localhost:3000`
- **THEN** the page SHALL load in a local sidecar and no request for it SHALL reach Browserbase

### Requirement: Real-profile browsing
When `browser.use_real_profile = true` (default false) and the session holds the `browser.real_profile` grant, the system SHALL copy the user's default Chromium-family browser's active profile (or the directory named by `browser.real_profile_pin`) into a managed snapshot under `<profile>/cache/browser/real-profile/<browser>/` using SQLite online backup for databases, re-sync it when a new browser session launches, launch the user's real browser binary on the snapshot (never the live profile directory), and run it headless unless `browser.headed`; if the copy is impossible (locked database, unsupported default browser, missing pin) the session SHALL get a throwaway profile and its first result SHALL carry `real_profile_unavailable: <reason>`; with `browser.real_profile_autoclose = true` the agent MAY offer, and only after user approval run, `prometheus browser close-profile`, which terminates the browser process tree bound to that profile; turning the setting off SHALL delete the snapshot on next browser use; unattended contexts SHALL use the snapshot without prompting.
**Milestone:** M2

#### Scenario: Missing pin
- **GIVEN** `browser.real_profile_pin = "Profile 9"` that does not exist
- **WHEN** a browser session starts
- **THEN** the session SHALL use a throwaway profile and report `real_profile_unavailable` naming the pin

#### Scenario: Revoking consent
- **GIVEN** an existing snapshot and `browser.use_real_profile` changed to false
- **WHEN** the next browser session starts
- **THEN** the snapshot directory SHALL be deleted

### Requirement: Lightpanda engine
`browser.engine` SHALL accept `auto` (default, Chromium) or `lightpanda`; with `lightpanda` and a `lightpanda` binary available (managed runtime or `PATH`) the system SHALL launch one `lightpanda serve --host 127.0.0.1 --port <free>` process per browser session, reap it with the same idle and crash rules as Chromium, and tell the model in the `browser_exec` description that screenshots are unavailable and only one page is supported; the engine setting SHALL have the lowest precedence (an explicit CDP endpoint, a cloud provider or real-profile browsing shadow it), and `prometheus browser status` and `prometheus doctor` SHALL report when it is shadowed.
**Milestone:** M3

#### Scenario: Shadowed by a cloud provider
- **GIVEN** `browser.engine = "lightpanda"` and `browser.provider = "browserbase"`
- **WHEN** `prometheus doctor` runs
- **THEN** it SHALL report that `browser.engine` is shadowed by `browser.provider`

### Requirement: Connecting to the user's browser
The interactive CLI and TUI SHALL provide `/browser connect [<ws-or-http-url>]`, `/browser status` and `/browser disconnect`; `connect` without a URL SHALL use `http://127.0.0.1:9222` and, if nothing listens there, SHALL launch a detected Chromium-family browser (Chrome, Brave, Edge, Chromium) with `--remote-debugging-port=9222` and a dedicated `--user-data-dir` under `<profile>/cache/browser/connect/` (required by Chrome 136+ for debugging); `status` SHALL show the endpoint, browser version and open targets; `disconnect` SHALL detach without closing the user's browser and return to the configured provider; gateway surfaces SHALL treat `/browser` as unavailable with a message rather than sending it to the model, and `prometheus browser status|close-profile` SHALL exist on the command line.
**Milestone:** M2

#### Scenario: Disconnect keeps the browser
- **GIVEN** a session connected to the user's running Chrome
- **WHEN** the user runs `/browser disconnect`
- **THEN** Chrome SHALL keep running and the next `browser_exec` SHALL use the configured provider

### Requirement: Bot screen
On Linux hosts without a physical display the system SHALL provide a per-profile virtual desktop (Xvnc plus a minimal Xfce session, geometry `bot_desktop.geometry` default `1440x900`) on which `computer_use` and headed browsers act, managed by `prometheus computer-use screen status|start|stop [--force]|install [-y]` and streamed to the desktop app over the RFB protocol through a gateway WebSocket that requires a single-use 30-second display ticket; it SHALL start only on explicit start or, when `bot_desktop.auto_start = true` (default false), on the first `computer_use` call or headed browser launch; it SHALL refuse to start with less than `bot_desktop.min_free_memory_bytes` (default 1610612736, i.e. 1536 MiB; 0 disables) free memory on the host or its cgroup, SHALL stop after `bot_desktop.idle_stop_secs` (default 1800, 0 disables) without use, and `install` SHALL run the distro package manager only after showing the exact package list (or print the command when not root and without `sudo`); `bot_desktop.placement` SHALL be `auto` (default: inside a container or SSH backend, on the gateway host for `local`, refused for backends that cannot host a display), `terminal` or `gateway` (explicit opt-in to act outside the terminal sandbox); while the screen runs, the bot's browser and the desktop's browser launcher SHALL share one persistent per-profile user-data directory.
**Milestone:** M3

#### Scenario: Low memory
- **GIVEN** 900 MB free memory and default config
- **WHEN** `prometheus computer-use screen start` runs
- **THEN** it SHALL refuse naming the 1536 MB threshold and exit 69

#### Scenario: Sandbox that cannot host a display
- **GIVEN** `terminal.backend = "modal"` and `bot_desktop.placement = "auto"`
- **WHEN** the screen is started
- **THEN** it SHALL refuse and point to `bot_desktop.placement = "gateway"` instead of starting on the host

### Requirement: Bot screen takeover lease
A viewer SHALL be able to take over and hand back control of a bot screen through RPC `screen.takeover` and `screen.handback`; while a human holds the lease the profile's `computer_use` and browser tools SHALL be refused with `human_has_control` (captures included), the most recent takeover SHALL win, a dropped viewer connection SHALL keep the lease, `screen.handback {force: true}` and `screen stop --force` SHALL clear it, a lease unused for 4 idle windows and at least 10 minutes SHALL expire back to the agent, and results of tool calls started before a takeover SHALL be voided by the lease epoch; `SECURITY.md` SHALL document that the screen, X display and browser debugging port are reachable by any process of the gateway's OS user, so the lease is a tool-level fence, not an OS boundary.
**Milestone:** M3

#### Scenario: Agent during takeover
- **GIVEN** a human holding the screen lease
- **WHEN** the agent calls `computer_use(action="capture")`
- **THEN** the call SHALL fail with `human_has_control`

### Requirement: Snapshot triggers, caps and per-run toggle
File snapshots SHALL be enabled by `files.snapshot.enabled` (default true) and overridable per invocation with `prometheus chat --checkpoints` or `--no-checkpoints`; besides file-tool writes, when `files.snapshot.terminal` is not `off` the system SHALL also checkpoint before terminal commands matching destructive shapes (`rm`, `rmdir`, `cp`, `mv`, `install`, `sed -i`, `truncate`, `dd`, `shred`, output redirection `>`, `git reset`, `git clean`, `git checkout`, `git restore`), taking at most one checkpoint per directory per turn; files larger than `files.snapshot.max_file_bytes` (default 10485760) SHALL be recorded as skipped rather than stored, each workspace SHALL keep at most `files.snapshot.max_checkpoints` (default 20) checkpoints, a nested git repository inside the workspace SHALL be checkpointed as its own project, and checkpointing SHALL never fail or block the triggering call.
**Milestone:** M2

#### Scenario: One checkpoint per directory per turn
- **GIVEN** a turn in which the model patches three files in `src/`
- **WHEN** the turn ends
- **THEN** exactly one checkpoint SHALL exist for `src/`'s project for that turn

#### Scenario: Large file
- **GIVEN** a 2 GB dataset in the workspace
- **WHEN** a destructive command triggers a checkpoint
- **THEN** the dataset SHALL be listed as skipped and the command SHALL run

### Requirement: Rollback listing, diff and selective restore
`/rollback` SHALL list the session workspace's checkpoints numbered newest first with time, triggering tool and files/lines changed; `/rollback <N>` SHALL restore the files changed since checkpoint N while leaving files the user edited after the agent's last write untouched and listed as `kept (hand-edited)`, SHALL append a note that also marks the latest turn as undone, and SHALL never rewrite earlier messages; `/rollback <N> --all` SHALL overwrite hand-edits too; `/rollback diff <N>` SHALL show the unified diff between checkpoint N and the current state without changing anything; `/rollback <N> <file>` SHALL restore only that file; the same operations SHALL be available as `prometheus files rollback` and RPC `files.rollback`.
**Milestone:** M2

#### Scenario: Hand-edit preserved
- **GIVEN** checkpoint 1 before the agent edited `a.rs` and `b.rs`, and the user later edited `b.rs`
- **WHEN** the user runs `/rollback 1`
- **THEN** `a.rs` SHALL be restored and `b.rs` SHALL keep the user's edit and be reported as kept

#### Scenario: Preview only
- **WHEN** the user runs `/rollback diff 2`
- **THEN** a diff SHALL be shown and no file SHALL change

### Requirement: Snapshot store management CLI
`prometheus files snapshots status` (alias `list`, `--limit <n>` default 20) SHALL print the store's total size, workspace count and per-workspace size, checkpoint count and last use; `prometheus files snapshots prune [--retention-days <n>] [--max-mb <n>] [--keep-orphans]` SHALL delete workspaces unused beyond the retention (default 7 days), enforce the size cap oldest first, and delete orphans (workspace directory missing) unless `--keep-orphans`; the automatic background prune (at most once per `files.snapshot.prune_interval_secs`, default 86400, never on the startup path) SHALL never delete orphans; `prometheus files snapshots clear [-f]` SHALL delete the whole store after confirmation, exiting 4 when confirmation is required but stdin is not a TTY.
**Milestone:** M2

#### Scenario: Unmounted volume
- **GIVEN** snapshots for a workspace on an external disk that is not mounted
- **WHEN** the automatic prune runs
- **THEN** that workspace's snapshots SHALL be kept

### Requirement: Working-tree diff command
`/diff [unstaged|staged|all|session] [--stat] [<path>...]` SHALL show, for the session cwd's git repository, the unstaged plus untracked diff (default), the staged diff, both, or `session`: the cumulative diff of every file the agent changed in this session from its earliest snapshot (available outside git too); `--stat` SHALL show per-file counts only; on gateway surfaces the output SHALL be fenced and truncated to the platform's message limit with a note giving the omitted line count; outside a git repository the non-session modes SHALL reply `not a git repository`.
**Milestone:** M2

#### Scenario: Session diff
- **GIVEN** a session that patched `a.rs` and created `b.rs`
- **WHEN** the user runs `/diff session --stat`
- **THEN** the output SHALL list `a.rs` and `b.rs` with added and removed line counts

### Requirement: Project run-recipe verification
`prometheus verify [PATH] [--detect-only] [--save] [--phase bootstrap|build|test|start]... [--skip-start] [--port <n>] [--timeout <secs>] [--ready-timeout <secs>] [--json]` SHALL detect a project's recipe from its manifests (Cargo, package.json with its lockfile's package manager, pyproject, go.mod, Makefile), with `<project>/.prometheus/environment.json` overriding detection; SHALL run the selected phases in order on the session's backend under the sandbox with a per-phase timeout (default 600 seconds), start the `start` phase in the background and poll the port (from `--port` or the recipe) until ready or `--ready-timeout` (default 60) expires, then stop it; SHALL print per-phase status, duration and an output tail; `--detect-only` SHALL print the recipe as JSON without running anything, `--save` SHALL write it to `environment.json`; it SHALL exit 0 when every phase passes and 1 otherwise, and SHALL record the outcome as verification evidence for the current session.
**Milestone:** M2

#### Scenario: Detect only
- **GIVEN** a Rust project with a `Cargo.toml`
- **WHEN** `prometheus verify --detect-only` runs
- **THEN** it SHALL print JSON containing `cargo build` and `cargo test` phases and run nothing

#### Scenario: Failing tests
- **GIVEN** a project whose test phase exits non-zero
- **WHEN** `prometheus verify` runs
- **THEN** it SHALL report the test phase failed with its output tail and exit 1
