# Chatting with the agent

`prometheus` with no subcommand is `prometheus chat`. On a terminal (stdin
and stdout both terminals) it opens the line interface; anywhere else, or
with `-z`, it runs one turn and prints only the answer. `ui.interface`
and `--tui`/`--line` choose the interactive interface; this build ships
the line interface, which also stands in for `tui`. A pipe is always
one-shot, whatever the configuration says.

## One-shot runs

The prompt comes from `-z <prompt>`, `--prompt-file <path>` (`-` for
stdin) or a pipe. With text on `-z`, piped stdin is appended to it as
context. Only the answer goes to stdout; with `--json` it is one object
with `session_id`, `answer`, `usage` and `exit_reason`.

```console
$ prometheus config set model.provider mock
$ prometheus config set model.default mock
$ prometheus -z "hello"
mock: hello
$ prometheus chat --provider mock -z "hi" --json
{"session_id":"<id>","answer":"mock: hi","usage":<usage>,"exit_reason":"completed"}
```

The exit status follows the shared table: `0` success, `2` usage
error, `3` a budget ran out before a final answer, `4` a tool call needed
an approval nobody could give (`approvals.single_query_mode = "deny"`, the
default; `--approvals off` or `"approve"` lets it run), `66` a missing
`--prompt-file` or nothing to continue, `78` no model configured, `130`
SIGINT and `143` SIGTERM. `--query`, `--prompt-file` and `-z <prompt>`
exclude each other.

```console
$ prometheus -z "a" --prompt-file notes.md # exits 2
error: --query, --prompt-file and -z <prompt> cannot be used together
```

A prompt file is sent verbatim: a body that starts with `/` or `!` is
text for the model, never a command.

## Flags of the session

`-m/--model`, `--provider`, `-t/--toolsets`, `--approvals`, `--yolo`,
`--max-iterations`, `--run-budget <secs>`, `-s/--skills`, `--source`,
`--pass-session-id`, `--no-context-files`, `--mock-script`, `--base-url`
and `--api-key-stdin` apply to the started session only and are never
saved.

## Resuming

`-r/--resume <ref>` takes an id, an id prefix, a title (a bare title
picks its newest `#n` session) or `latest`. `-c` continues the newest CLI
session started in the current directory, preferring the one last used
from the same terminal (`sessions.terminal_continue`); `-c <name>`
continues the newest session of the lineage titled `<name>`. A resumed
session moves to its recorded directory (`cwd restored to <dir>`) unless
`--cwd <dir>` is given, which also scopes `-c` and `latest`. Nothing to
continue exits 66 with `no session to continue in <dir>`. An interactive
resume prints a recap of the last exchanges (`display.resume_display`,
`resume_exchanges` and the `resume_max_*` limits); it is never sent to
the model.

```console
$ prometheus -c -z "again" # exits 66
error: no session to continue in <dir>
```

## The line interface

A trailing `\` or Alt+Enter continues a line. History is kept in
`<profile>/cli_history` (10,000 entries, owner-only, secrets redacted).
`@` completes files under the working directory. Ctrl+C stops a running
turn (its transcript so far is kept); at an idle prompt a second Ctrl+C
within two seconds, or Ctrl+D, exits. `--query <text>` starts the session
with `<text>` as its first message, verbatim. `-q` drops the banner and
progress lines.

Slash commands come from one registry; `/help` lists the ones available
here, grouped by category. The command word is case-insensitive (its
arguments keep their case), a unique prefix runs its command (`/comp` is
`/compress`) and a typo gets up to three suggestions. While a turn runs,
each command follows its busy policy: most are refused with `/<name> is
unavailable while the agent is working; use /stop first`, `/stop`, `/new`
and `/exit` stop the turn first, and read-only ones such as `/help` and
`/usage` answer at once. Plain text typed meanwhile follows
`busy.input_mode`: `queue` (default) sends it after the turn, `steer`
hands it to the running turn, `interrupt` stops the turn and sends it.

`/clear`, `/new`, `/undo` and `/rewind` ask `once / always / cancel`
first (`always` sets `cli.confirm_destructive = false`; `--yes` skips the
question). Changes that would rebuild the system prompt, such as `/tools
disable web`, `/skills` and `/memory` changes, apply to the next session;
add `--now` to rebuild the current session's prompt and tools at the next
turn (one `prompt.rebuilt` event, `reason` `<command> --now`).

`/<skill> [instruction]` loads skills into one user message (up to five
stacked: `/pdf /summarise report.pdf`). User commands live in
`[quick_commands.<name>]`: `type = "alias"` re-dispatches `target` with
your arguments appended; `type = "exec"` runs `command` on the session's
sandboxed terminal without any secret in its environment, output
redacted, killed after `timeout_secs` (default 30). Built-in commands win
over quick commands, which win over plugin and skill commands; a name two
of those share is logged once as a WARN at startup.

```toml
[quick_commands.ship]
type = "alias"
target = "/title"

[quick_commands.status]
type = "exec"
command = "git status --short"
timeout_secs = 10
```

## Themes

`display.theme` picks `default`, `mono`, `daylight`, `slate` or a file
`<profile>/themes/<name>.toml`, which may `extends` a built-in and
override some colours (`prompt`, `assistant`, `tool`, `notice`, `error`,
`accent`; a colour is a name, `bright_<name>`, `#rrggbb`, `bold`, `dim`,
`italic` or `underline`). A user theme named like a built-in is ignored
with a WARN. `/theme` lists the themes; `/theme <name>` switches and saves
it. Colour is off with `--no-color`, `NO_COLOR` or a stdout that is not a
terminal.

```toml
extends = "slate"
[colors]
prompt = "#88c0d0"
error = "bright_red"
```

## Usage

`/usage` shows calls, input, output, cache-read, cache-write and
reasoning tokens and cost for the session and for its whole lineage
(compression continuations, forks and delegated children), then the
configured budgets.
