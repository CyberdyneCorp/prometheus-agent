# The local terminal

On the `local` backend every [`terminal`](../reference/tools.md#terminal) command runs in a fresh,
non-interactive, non-login shell confined by the session's OS sandbox (see
[OS sandbox and terminal backends](sandbox.md)). This page describes what
the shell sees and how results come back.

## Where commands run

- The session starts in `terminal.cwd` (`~` expanded), else the gateway
  session's workspace, else the directory `prometheus` was started from.
- A command that ends in another directory (`cd sub`) moves the session; the
  result then carries `cwd`. A per-call `workdir` never moves the session.
- A path that looks like this machine's (`/Users/...`, `/home/...`, `C:\...`)
  configured for a remote backend is replaced by that backend's default
  directory, with a notice naming the ignored path.

## The child environment

A child never inherits Prometheus' environment. It gets:

- the allowlist `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TERM`,
  `SHELL`, `TZ` and the platform location variables (XDG directories;
  `SystemRoot`, `APPDATA`, `ProgramFiles`... on Windows), with their launch
  values;
- `TMPDIR` pointing at the session scratch directory;
- `PROMETHEUS_SESSION_ID`, the session's id (`prometheus verify` records its
  outcome for that session);
- the names in `terminal.env_passthrough`, with their launch values;
- only the secrets the target declares AND the session holds a
  `secrets.read:<NAME>` grant for.

`terminal.env_passthrough` (and `terminal.container.env`) must not name a
secret: a stored secret, a provider credential, a `PMT_SECRET_*` variable or a
credential-shaped name (`*_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`...).
Loading such a profile fails with exit 78:

```console
$ prometheus config set terminal.env_passthrough '["OPENAI_API_KEY"]'
$ prometheus config validate   # exits 78
error: terminal.env_passthrough: 'OPENAI_API_KEY' is a secret; grant it with secrets.read instead
$ prometheus config set terminal.env_passthrough '["JAVA_HOME"]'
$ prometheus config validate
<...>config.toml is valid
```

## Shell startup files

With `terminal.auto_source_bashrc = true` (default) and bash, `~/.bashrc` is
sourced once when the session's shell environment is created, then every
file in `terminal.shell_init_files` (missing files are skipped with a
one-time notice). Startup files run with stdin closed and are bounded by
`terminal.shell_init_timeout_secs` (default 10). When that passes, commands
run without the startup files and the first result carries:

```text
shell init timed out; guard interactive-only code in your rc file with: case $- in *i*) ;; *) return;; esac
```

The resulting exported variables, functions and aliases are kept in a
snapshot in the scratch directory; with `terminal.persistent_shell = true`
each command's `export`s and functions carry over to the next command.
Secrets never enter the snapshot.

## Foreground, background and limits

- Each call is checked first: `pty` and the other background-only options
  need `background=true`; `timeout` must be positive. The capability check,
  the approval gate and the workdir check must all finish within
  `max(timeout, 30)` seconds or the command is not run.
- A trailing `&` runs the command in the background. A foreground command
  that detaches (`nohup`, `disown`, `setsid`, an inline `&`) or starts a dev
  server or watcher (`npm run dev`, `vite`, `uvicorn`, `cargo watch`,
  `docker compose up`...) is refused with a hint to resend it with
  `background=true`; `--help` and `--version` calls are exempt.
- A foreground `timeout` above `terminal.max_foreground_secs` (600) starts a
  tracked background process instead (`promoted_from_foreground`) and you are
  notified when it ends. At the timeout the whole process group is stopped
  (SIGTERM, then SIGKILL after `tools.kill_grace_secs`).
- Output is ANSI-stripped, redacted and cut to `tool_output.max_chars`
  (50,000) keeping 40% head and 60% tail around one
  `... [OUTPUT TRUNCATED - <n> chars omitted out of <total> total] ...`
  marker. The full redacted output is then written to
  `<profile>/cache/terminal-spill/<session>/` (mode 0600) and returned as
  `full_output_path`; spill files are pruned after 7 days.
- If the execution environment itself fails (for example the shell is
  missing), the result is `status = "degraded"` with a `reason` and a
  `retry_hint`, and the next call rebuilds the environment
  (`environment_recreated`). `terminal.degraded_mode = "fail"` returns an
  error instead.

## Background processes

The [`process`](../reference/tools.md#process) tool lists, polls, logs, waits for, kills and writes to a
session's background processes (`list`, `poll`, `log`, `wait`, `kill`,
`write`, `submit`, `close`, `handoff`). Ids can be shortened to any unique
prefix. A session tracks at most 64 processes (the oldest finished one
makes room), keeps the last 200,000 characters of each one's output, and
forgets a process 30 minutes after it finished. When the session ends every
process group is stopped, so nothing is left running. `pty = true` is
accepted but not implemented yet: the process runs with pipes and the
result says so.

**Receipts.** When a process ends, a receipt (command, exit code, start and
end time, the last 200,000 characters of its redacted output) is kept in the
profile's `state.db`: the newest 64 per profile, for at most 7 days. `poll`,
`log` and `wait` with the original id read it after the process left the
live list, including from a resumed session or a compression continuation
of the same conversation; `list` shows the conversation's receipts marked
`retained`. Any other conversation gets `unknown process id`, even with the
exact id.

**Notifications.** A background call can ask to be told about the process:
`notify_on_complete` reports its exit; `watch_patterns` reports output lines
containing one of the strings, at most once per 15 seconds per process
(after 8 reports, or 3 windows in a row that held a match back, watching
stops and the exit is reported instead); `heartbeat` (seconds, at least 60)
reports what the process printed since the previous heartbeat. A session
takes at most 15 such notifications per 10 seconds; past that, progress
notifications pause for 30 seconds (an exit is never dropped). On the line
interface each one is printed as its own `[background]` message; on a
messaging chat it arrives as a background notification
(`gateway.notifications.background`).

**Quiet waits.** With `terminal.process_wait_auto_notify = true` (default),
the second `poll` or `wait` in a row that finds a running process silent
switches its exit notification on and says `No new output; you will be
notified when it exits. Do other work or end your turn instead of polling.`
New output, the exit, or any other action on the process resets the count.
It never happens where a later notification cannot arrive: single-query
`chat -z` runs, cron runs, subagents, sessions without a notification route,
or a chat whose background notifications are `off`.

**Handoff.** A subagent can give a running process to its parent session
with `handoff` (at most 3 per subagent: `handoff cap reached (3 per
subagent); wait on or kill the rest yourself.`); the parent then owns it,
can poll and kill it, and is told when it exits. Other sessions get
`handoff is available only to subagent sessions`.

## Windows

`terminal.windows_shell = "auto"` uses Git Bash (`terminal.windows_bash_path`,
then the Git for Windows install locations, then `bash.exe` on `PATH`), but
never the WSL launcher (`System32\bash.exe`) or a `WindowsApps` alias. Without
Git Bash it falls back to `pwsh`, then Windows PowerShell, with a one-time
notice that POSIX syntax is unavailable; exports and directory changes then
do not carry over between commands. Selecting `git-bash`, `pwsh` or
`powershell` explicitly makes the terminal unavailable when that shell is
missing (`pwsh not found`, with an install hint). The CLI switches its
console to UTF-8 (code page 65001) and restores the original code pages on
exit.

Git Bash cannot run inside the Windows sandbox: its MSYS2 runtime creates
named objects under `\BaseNamedObjects` at startup, which an AppContainer
may not do. While the sandbox is enforced (the default), `auto` therefore
uses `pwsh`, then Windows PowerShell, with a notice saying why, and an
explicit `git-bash` makes the terminal unavailable (`git-bash cannot run
inside the Windows sandbox (AppContainer)`) rather than run unconfined.
PowerShell runs each command from the session's directory or fails with
exit code 126, with `-ExecutionPolicy Bypass` so the scripts the agent
writes can run (the sandbox, not the execution policy, is the boundary).
