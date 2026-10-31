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

`--image <path>` (repeatable) attaches a PNG, JPEG, GIF or WebP image of
at most 20 MiB to the first prompt. Images are kept in the profile's
blob store and sent as pixels, or, when the model cannot see
(`supports_vision = false`) and `[auxiliary.vision]` names a route, as
that route's description (`vision.image_input_mode` can force either).
A model without vision and without a vision route refuses before any
provider call: exit 78, `model <id> does not accept images`.

`--ignore-user-config` runs on schema defaults plus this command line's
flags and `--set` values instead of the profile's `config.toml` (secrets
still resolve). `--ignore-rules` leaves out workspace rule files
(`AGENTS.md` and friends), the identity file (`SOUL.md`), the memory
snapshot and preloaded skills. Both are recorded on the session:
`prometheus sessions show` prints `ignore_rules = true`.

`--usage-file <path>` writes one JSON document when the run ends, for
any reason (failure, SIGINT, SIGTERM included): `session_id`,
`root_session_id`, `model`, `provider`, `billing_source`, the
`input_tokens`, `output_tokens`, `cache_read_tokens`,
`cache_write_tokens` and `reasoning_tokens` of this run's calls, `cost`
with `cost_status`, `turns`, `exit_reason` and `exit_code`. It is written
atomically and independently of stdout; a path that cannot be written
exits 73.

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

Composing: `/image <path>` attaches an image to the next prompt (bare
`/image` lists the pending ones, `/image clear` drops them).
`/prompt [text]` (aliases `/compose`, `/edit`) and Ctrl+X Ctrl+E open
`$VISUAL`, then `$EDITOR`, then `vi` (`notepad` on Windows) on a private
Markdown file holding the text or the current buffer; what you save is
sent as your next message, and a file left empty prints `empty prompt;
nothing sent`. The file is deleted afterwards.

A line starting with `!` runs a shell command on the session's terminal
backend, under the same sandbox and approval rules as the `terminal`
tool; its output is shown here and never sent to the model. `!!<command>`
also sends the command and its output (redacted, at most 16 KiB) with
your next message. While a turn runs, `!` lines are refused with `shell
escape is unavailable while the agent is working`. Ctrl+Z at an idle
prompt suspends Prometheus (`fg` brings the prompt back with what you had
typed); while a turn runs it prints `cannot suspend while the agent is
working`.

Tool activity follows `display.tool_progress`: `off`, `new` (a line when
the tool changes), `all` (the default; `display.tool_progress_grouping =
"accumulate"` folds consecutive calls of one tool into `⚙ terminal ×3`,
`separate` prints each) or `verbose` (arguments and result previews,
cut to `display.tool_preview_length` characters; 0 means no limit).
`/verbose` cycles the modes for this session. `/focus on` hides tool
lines (one `N tool lines hidden` line per turn, a `focus` badge on the
prompt) and `/focus off` restores the previous mode. After each turn a
summary line shows its duration, tool calls and tokens
(`display.turn_summary`; `display.show_cost` adds the cost).
`display.interim_messages` shows assistant text written between tool
calls, `display.show_reasoning` readable reasoning,
`display.bell_on_prompt` rings the terminal bell when an approval or
clarify question opens and `display.bell_on_complete` when a turn ends.
`/timestamps on|off|status` (alias `/ts`) prefixes answers with the time
(`display.timestamp_format`, default `%H:%M`) and saves
`display.timestamps`.

Local commands never call a model: `/history [N]` prints the last N
user and assistant messages (`[HH:MM]` prefixes when timestamps are on),
`/status` the model, provider, profile, session id, title, working
directory, start and last-activity times, token and cost totals, whether
a turn runs and a recap (turns, top five tools, files touched, the
latest prompt and reply cut to 200 characters), `/version` the
`prometheus --version` line, and `/sessions` the ten most recent
sessions of this directory with a `(current)` marker (`/sessions all`
for the whole profile, `/sessions search <query>` for full-text hits).
On messaging surfaces `/sessions all` is reserved for admins.

`/retry` sends the last message again with its attachments; the answer
and everything after the message leave the active transcript (a soft
delete, so `sessions rewind` can bring them back). While a turn runs it
stops the turn first; with no message it prints `nothing to retry`.
`/queue <prompt>` (alias `/q`) runs a prompt after the current turn (at
once when idle); `/queue list`, `/queue edit <n> <text>`, `/queue rm
<n>`, `/queue move <a> <b>`, `/queue clear` and `/queue add <text>`
manage the queue, with 1-based indexes (`no queued item <n>` otherwise).
Text typed during a turn with `busy.input_mode = "queue"` joins the same
queue.

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
