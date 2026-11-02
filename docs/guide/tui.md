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

From top to bottom: the startup banner and the transcript (your prompts,
tool calls, notices, the assistant's answers), a card when the agent asks
something, the completion list (or the palette or session switcher), the
composer, and the status line with the profile, the model, the context
window's use, the session's cost, the approvals waiting and, with more
than one live session, `<n> live`. It stays usable at 40 columns by 12
rows. `NO_COLOR` (or `--no-color`) drops colours.

`ui.tui.theme` picks `dark` or `light`; with `auto` (the default) the TUI
follows `COLORFGBG` when it is set, else asks the terminal for its
background colour (an OSC 11 query answered within 100 ms), else uses
dark. `/theme` shows the theme in use and where it came from, `/theme
light` switches for this run.

While a turn runs the status line leads with a busy indicator in the
`ui.tui.busy_indicator` style (`kaomoji`, `emoji`, `unicode` or `ascii`),
changing every 2.5 s; `/indicator <style>` switches and saves it, and
`ui.tui.reduced_motion = true` keeps it still.

The startup banner lists what the session runs with in sections: Tools
(open), Skills, System Prompt and MCP Servers (closed). Click a header, or
press `Ctrl+B` and then `Up`/`Down` and `Enter`, to open or close one; the
next launch starts from the defaults again.

Answers render inline `$...$` and block `$$...$$` math as Unicode text
(`$x^2$` shows `x²`); dollar amounts stay as they are, and math the TUI
cannot format is shown as the TeX itself in a code span. The line
interface prints the TeX unchanged.

## Display commands

These change only this screen and never reach the daemon; the toggles are
saved in the profile's `config.toml`:

- `/redraw` clears and repaints the screen (after something else wrote
  over it);
- `/statusbar` (or `/sb`) hides or shows the status line
  (`ui.status_bar`);
- `/battery on|off|status` shows the battery level, coloured by charge, as
  the first status item (`ui.status_bar_battery`); a machine without a
  battery shows none, and `/battery status` says `no battery detected`.

The line interface runs the same three itself: its status is a single
bottom line on the terminal (profile and model, and the battery when on),
and `/redraw` clears the screen and prints the prompt and status again.

An approval card is answered with `o` (once), `s` (session), `a` (always)
or `d` (deny) on an empty composer; a question card with a choice number,
your own text, or an empty line to skip. When the agent needs a secret
(a skill's missing key) or the sudo password, the card takes masked
input: what you type is shown only as bullets, never enters the input
history or the transcript, and is sent with Enter; Esc, Ctrl+C or an
empty input declines. Each card counts down to its deadline; a card
answered in another window, timed out or left without anyone to answer
is withdrawn.

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

`Ctrl+P` (or `/palette`) opens the command palette: type part of a
command's name and the commands this screen runs, the installed skills
and the TUI's own commands are ranked by how well they match, each with
its description and where it runs. `Enter` puts the chosen command in the
composer (it does not run it), `Esc` closes the palette.

A paste of at least `ui.composer.paste_collapse_lines` lines (5) or
`ui.composer.paste_collapse_chars` characters (2000) shows as one chip,
`[Pasted text #1: 300 lines]`; `Backspace` deletes it whole, `Ctrl+O`
expands it back into its text, and the prompt sent carries the full text
(`0` turns a threshold off). `/paste`, or `Ctrl+V` when the clipboard holds
an image and no text, attaches the clipboard's image to the next prompt as
`[Image #1]` (`Backspace` removes it); the image is kept under
`<profile>/cache/clipboard/` and stored in the profile's blob store with
the prompt. The clipboard is read with the platform's tool: the macOS
pasteboard (`osascript`), PowerShell on Windows and from WSL2, `wl-paste`
on Wayland, `xclip` on X11 (`xsel` reads text only). Where none is
reachable, over SSH for example, the TUI says why and asks for an `@path`
instead. The integrated terminals of VS Code, Cursor and Windsurf keep
`Ctrl+V` for themselves: `/terminal-setup` shows the keybinding that sends
it to the TUI and, once you answer `y`, adds it to each installed editor's
`keybindings.json` (the old file is kept as `keybindings.json.bak`).

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
clipboard escape), `/theme [dark|light]`, `/keys`, `/redraw`,
`/statusbar`, `/battery`, `/indicator`, `/palette`, `/paste`,
`/terminal-setup`, `/sessions` (`/switch`), `/new`, `/resume <ref>`,
`/fork` and `/update`. Every other command of the session (the
built-ins, `[quick_commands]` and skills) runs in the daemon, and a skill
command's message is sent as a prompt. Text whose first word is not a
command, such as `/var/log/syslog is huge`, is a prompt.

`/update` leaves the TUI, restores the terminal and runs `prometheus
update` in the same process; the TUI exits with the updater's status.

## Several live sessions

A TUI can hold several sessions at once; each keeps running in the daemon
while another is on screen. `/new`, `/resume <ref>` and `/fork` show the
session they open and keep the previous one live. `Ctrl+X`, `/sessions`
(`/switch`) or a click on the `<n> live` status item opens the switcher:
`Up`/`Down` select, `Enter` shows the selected session, `Ctrl+D` detaches
from it and drops it from the list (the daemon closes it once idle),
`Ctrl+N` (or `/sessions new`) starts a new one, `Ctrl+R` refreshes the
titles and `Esc` closes the switcher. On the last row, `+new`, type a
prompt and press `Enter` to start a session with it; `Tab` first picks
that session's model.

With `ui.tui.auto_resume = "recent"` a TUI started without `--resume`,
`--continue` or a prompt comes back to the session last shown in the TUI
from the same directory (kept in `<profile>/tui/recent.json`), for
example after an SSH link dropped and took the terminal with it; when
that session is gone it starts a new one, and `--resume` always wins.

## Leaving and crashes

Leaving, a signal (`SIGINT` exits 130, `SIGTERM` and `SIGHUP` 143) and a
crash all give the terminal back in the state it had: normal mode, the
primary screen, the cursor shown, mouse capture and bracketed paste off. A
crash writes its report to `<home>/logs/tui-panic-<timestamp>.log`, prints
that path and exits 70. When the connection to the daemon drops (a
restart), the TUI reconnects to every live session and catches up.
