# Editors over ACP

`prometheus acp` (also `pmt acp`) is the agent as an [Agent Client
Protocol](https://agentclientprotocol.com) server: Zed, VS Code, JetBrains
and other ACP clients launch it and talk newline-delimited JSON-RPC on its
stdin and stdout.

The process is a *viewer* of the profile's gateway daemon, never the place
where turns run. An editor session is a daemon session created with source
`acp`: its prompts are daemon admissions, its approvals and questions are
the daemon's requests shown as editor cards, and its events come back as
`session/update`s. The same session is visible to the TUI, the desktop and
every other client; closing the editor only detaches, and a running turn
keeps going (load the session again to see how it ended).

## Launching

```console
$ prometheus acp --check # exits 1
no default model configured for profile default; run 'prometheus setup'
$ prometheus config set model.provider mock
$ prometheus config set model.default mock
$ prometheus acp --check
Prometheus ACP check OK
```

`--check` verifies that the profile resolves, that the daemon runs or may
be started (`daemon.autostart`, default true), that a model is configured
and, when `toolsets.surfaces.acp` asks for the browser tools, that the
managed Chromium is installed; it prints the first failing check and exits
1. `--version` prints the version, `--setup` runs `prometheus setup` and
then offers the browser runtime, and `--setup-browser [--yes]` installs the
managed Chromium (asking first unless `--yes`; already installed is
success). All of them exit without starting the server.

Configuration comes from the profile (`--profile`, else the active one)
and its secrets store, plus the launch flags `--model`, `--provider`,
`--yolo`, `--safe-mode` (which reads no profile default model, so it needs
`--model` and `--provider`) and `--ignore-user-config`. No other
environment variable changes what the adapter does.

The process connects to the daemon of its home with a ticket for its
launch profile only; a daemon that does not serve that profile is refused
with `profile_mismatch`. Stdout carries only protocol frames: the process
writes them to a private copy of stdout and points its own stdout (and so
every child's) at stderr. Logs go to stderr and to
`<profile>/logs/acp.log` (`prometheus logs acp`), secrets redacted.
`ping`, `health` and `healthcheck` probes are answered `-32601` without a
warning.

## Sessions

`initialize` answers the protocol version, `agentInfo` (`prometheus` and
the version `prometheus --version` prints) and the capabilities: session
loading, images in prompts, and session `list`, `fork` and `resume`. It
lists the terminal sign-in `prometheus-setup` (it runs `prometheus setup`)
and, when the profile's default provider has a credential, that provider;
`authenticate` succeeds only for a listed method whose credential resolves.

- `session/new` creates the daemon session in the editor's cwd with the
  editor's MCP servers, and answers its models, its edit modes (starting at
  `default`) and its config options, then sends the available commands and
  a usage update.
- `session/load` and `session/resume` attach the session an editor-held id
  continues in (following compression continuations), replay its
  conversation, and re-offer approvals still waiting. An idle session moves
  to the editor's cwd; a busy one says `Attached to the running session;
  editor cwd is not applied.`
- `session/list` lists the profile's ACP sessions with messages, one per
  conversation, newest first, 50 a page.
- `session/fork` branches the session in the daemon, in the editor's cwd,
  keeping the parent's editor MCP servers unless others are given.

Editor MCP servers are run by the daemon for that session, under the
profile's sandbox; a later editor that sends none keeps them, one that
sends others replaces them from the next turn (`Editor MCP tools changed;
the new tool list applies from the next turn.`). Definitions holding
`${...}` placeholders and the legacy `sse` transport are skipped.

## Prompts

A prompt's text, images and file links become one daemon prompt: files
are inlined up to 512 KiB (longer ones truncated, binary ones omitted, each
with a note), images are staged in the profile's blob store when their
bytes are PNG, JPEG, GIF or WebP (anything else becomes `[Image omitted:
unsupported image format]`), remote image URLs are fetched under the
profile's egress policy, and a prompt carries at most 10 attachments.

The first word of a prompt is a command only when it names one of the
registry's commands for ACP (they are advertised after every attach);
`/var/log/x is wrong` is prompt text. A refused command is shown as an
agent message and the prompt ends normally.

`session/prompt` returns `end_turn`, `cancelled`, `max_tokens` or
`max_turn_requests`; a turn that failed is a JSON-RPC error
`admitted_turn_failed: <reason>`. `session/cancel` stops only the turn this
editor submitted, never one another client started. A prompt whose
acknowledgement was lost is resent exactly once: the same prompt reuses its
input id, so the daemon answers with the original admission. When the
daemon connection is lost during a prompt, the prompt fails with `Gateway
disconnected; turn outcome is unknown...`; new prompts in that session are
refused (`unknown_execution`) until the outcome is known from the session
or you send `/discard <admission_id>`.

The answer streams as message chunks (the final text adds only what was not
streamed yet), reasoning as thought chunks, the todo list as a plan, and
every tool call ends `completed` or `failed`. Title, usage (with the
conversation's cost, subagents included) and changes made from other
clients (model, effort, edit mode) are sent as they happen.

## Approvals, modes and options

Approvals become permission cards offering only the choices the request
allows; a file edit shows as a diff. Clarify questions use the editor's
forms when it has them, else one card per question with a `Skip` option.
Dismissing a card withdraws only this editor's answer: another attached
client may still answer, and with none left the approval is denied (the
question unanswered) at once.

The modes are `default` (ask before edits), `accept_edits` (edits inside
the workspace run without asking) and `dont_ask`; they change only while
no turn runs. The config options are `edit_approval_policy`, `model` and
`reasoning_effort`; an unknown option or value is refused with `-32602`.
`session/set_model` takes `<provider>/<model>`; during a turn it applies
from the next one.

## Editor files and terminals

When the editor offers file reads and writes, the file tools read and
write files under the session's cwd through the editor, so unsaved buffers
are seen and edits land in open buffers; other files use the daemon's own
tools, and an editor error is the tool's error. With
`acp.use_editor_terminal = true` and an editor that offers terminals,
terminal commands of a local session run in the editor's terminal panel —
only while `security.sandbox.mode = "off"`, because the editor terminal is
outside the OS sandbox; otherwise they run sandboxed in the daemon.

The browser tools are part of an ACP session only when its toolsets include
`browser` and the managed Chromium is installed when the session starts
(`prometheus acp --setup-browser`).

## Relay bridges

`prometheus acp --unattended` serves clients that cannot show prompts,
such as channel relay bridges. It never sends them approvals or questions:
approvals follow `approvals.unattended_mode` (default `deny`) and the
profile's grants, its sessions are tagged `acp-unattended` and use the
`toolsets.surfaces.acp_unattended` toolset (by default the ACP tools
without terminal and code execution), and a permission answer such a
client sends anyway is ignored with a warning.
