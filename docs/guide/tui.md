# The terminal UI

`prometheus tui` opens the full-screen terminal UI, and so do `prometheus`
and `prometheus chat` on a terminal while `ui.interface = "tui"` (the
default). It needs stdin and stdout to be terminals:

```console
$ prometheus tui # exits 2
error: prometheus tui needs an interactive terminal (stdin is not a TTY)
```

## A client of the gateway daemon

The TUI owns only the screen. The session, its agent, its tools and its
approvals live in the home's gateway daemon ([The daemon and its client
protocol](client-protocol.md)), so a session keeps running when the TUI
closes and several windows can watch it. When the daemon is not running
the TUI starts it detached and attaches; with `autostart = false` under
`[daemon]` in `<home>/daemon.toml` it exits 69 instead, without touching
any session store. `prometheus tui --standalone` runs an in-process
gateway instead, holding the home for the TUI's lifetime and serving
nothing but that TUI; next to a running daemon it exits 75 naming it.

`-r <ref>` and `-c [name]` resume a session as in the line interface, and
`-m`/`--provider` pick the model of a new session. On `prometheus chat`
the session flags (`--set`, `--skills`, `--yolo`, `--image`, `--query`,
...) apply too, and `--in-process` means `--standalone`.

## The screen

From top to bottom: the transcript (your prompts, tool calls, notices,
the assistant's answers), a card when the agent asks something, the
completion list, the composer, and the status line with the profile, the
model, the context window's use, the session's cost and the approvals
waiting. It stays usable at 40 columns by 12 rows. `NO_COLOR` (or
`--no-color`) drops colours; `ui.tui.theme` picks `dark` or `light`
(`auto` is dark until terminal detection lands), and
`ui.tui.reduced_motion = true` replaces the busy spinner with a still
glyph.

An approval card is answered with `o` (once), `s` (session), `a` (always)
or `d` (deny) on an empty composer; a question card with a choice number,
your own text, or an empty line to skip. A card answered in another
window, timed out or left without anyone to answer is withdrawn.

Text typed while a turn runs follows `busy.input_mode`: it is queued and
runs next (`queue`), handed to the running turn after its current tool
step (`steer`), or stops the turn and runs at once (`interrupt`).

## Keys and the composer

`Enter` sends; `Alt+Enter` (or `Shift+Enter` where the terminal reports it)
starts a new line; pastes keep their lines. `Up`/`Down` walk the history,
kept per profile in `<profile>/tui/history` (1000 entries, credentials
redacted). `Tab` completes a `/command` or an `@path`, listed through the
session's own terminal backend. `Ctrl+C` stops the running turn (or
clears the composer), `Ctrl+D` on an empty composer leaves, `PageUp`,
`PageDown`, `Ctrl+Up` and `Ctrl+Down` scroll.

Every binding can be changed under `[ui.tui.keys]` (action name, then a
chord such as `Ctrl+B`, `Alt+Enter`, `PageUp` or `F2`); an unknown action
or a chord that does not parse stops the TUI with exit 78 naming the key,
and `/keys` lists the bindings in effect:

```toml
[ui.tui.keys]
page_up = "Ctrl+B"
page_down = "Ctrl+F"
```

## Slash commands

The TUI runs its own commands without asking the daemon: `/quit`, `/clear`
(the screen only), `/copy` (the last answer, through the terminal's
clipboard escape), `/theme [dark|light]`, `/keys`, `/new`, `/resume
<ref>`, `/fork` and `/update`. Every other command of the session (the
built-ins, `[quick_commands]` and skills) runs in the daemon, and a skill
command's message is sent as a prompt. Text whose first word is not a
command, such as `/var/log/syslog is huge`, is a prompt.

`/update` leaves the TUI, restores the terminal and runs `prometheus
update` in the same process; the TUI exits with the updater's status.

## Leaving and crashes

Leaving, a signal (`SIGINT` exits 130, `SIGTERM` and `SIGHUP` 143) and a
crash all give the terminal back in the state it had: normal mode, the
primary screen, the cursor shown, mouse capture and bracketed paste off. A
crash writes its report to `<home>/logs/tui-panic-<timestamp>.log`, prints
that path and exits 70. When the connection to the daemon drops (a
restart), the TUI reconnects to the same session and catches up.
