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
`write`, `submit`, `close`; `handoff` arrives with subagents in M2). Ids can
be shortened to any unique prefix. A session tracks at most 64 processes
(the oldest finished one makes room), keeps the last 200,000 characters of
each one's output, and forgets a process 30 minutes after it finished. When
the session ends every process group is stopped, so nothing is left running.
`pty = true` is accepted but not implemented yet: the process runs with
pipes and the result says so.

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
