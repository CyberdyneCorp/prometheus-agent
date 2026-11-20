# Pets and achievements

A pet is an optional animated mascot that mirrors what the agent is doing
in the line interface, the TUI and the desktop app. Achievements are
badges computed on your machine from the profile's session history. Both
are purely presentational: they are off until you turn them on, and they
never change a session's system prompt, tools, memory or token use.
Selecting, hiding or resizing a pet in the middle of a conversation does
not rebuild the session.

## Installing and selecting a pet

Pets come from the public petdex gallery (`pets.gallery_url`). Each one is
downloaded over HTTPS, its atlas checked (PNG or WebP of at most 8 MiB,
1536x1872 pixels: 8 columns by 9 rows of 192x208 cells, one row per
animation state) and stored under `<profile>/pets/<slug>/` with a
`pet.toml` recording its author, source, license and the atlas's SHA-256.
An atlas that fails a check is refused with `invalid pet atlas: <reason>`
and nothing is written.

| Command | What it does |
|---|---|
| `prometheus pets list [query] [--limit N]` | Gallery pets whose slug or name contains the query |
| `prometheus pets list --installed` | The installed pets (`*` marks the one shown) |
| `prometheus pets install <slug> [--select] [--force]` | Download and install a gallery pet; `--select` shows it |
| `prometheus pets select [slug]` | Show an installed pet (a picker on a terminal without a slug) |
| `prometheus pets off` | Hide the pet (`pets.enabled = false`) |
| `prometheus pets remove <slug>` | Delete a pet; removing the one shown turns pets off |
| `prometheus pets scale <factor>` | The size on every surface, 0.1 to 3.0 of the native cell |
| `prometheus pets show [slug] [--state s] [--cycle] [--once] [--mode m] [--scale f]` | Animate a pet in this terminal until Ctrl+C |
| `prometheus pets doctor` | What showing a pet needs; prints `ready` or the first missing item (exit 1) |
| `prometheus pets hatch <description>` | Generate a pet (see below) |

An unknown slug exits 66. Out-of-range scales are clamped and said so:

```console
$ prometheus pets scale 5
scale clamped to 3.0
$ prometheus pets doctor # exits 1
not ready: no pet is installed
$ prometheus pets off
pet hidden
```

## Configuration

| Key | Default | Meaning |
|---|---|---|
| `pets.enabled` | `false` | Show the pet |
| `pets.slug` | empty | The pet shown (empty: the first installed) |
| `pets.render_mode` | `auto` | `auto`, `kitty`, `iterm`, `sixel`, `unicode` or `off` |
| `pets.scale` | `0.33` | Size on every surface (0.1-3.0) |
| `pets.unicode_cols` | `0` | Width of a half-block pet in columns (0: from the scale) |
| `pets.roam` | `false` | On the desktop, wander while the agent is idle |
| `pets.image_provider` | empty | The image backend `/hatch` uses |
| `pets.gallery_url` | the petdex manifest | Where `pets list` and `install` look |

Every key applies at once on every surface: a running TUI or line
interface picks up a change made elsewhere within a second.

## In the terminal

The line interface draws the pet in a pane right-aligned above the
status line; the TUI draws it in the transcript's bottom-right corner.
With `render_mode = "auto"` the richest protocol the terminal is known to
speak is used (kitty for kitty, Ghostty and WezTerm; iTerm2 inline images;
sixel for foot, mlterm and mintty) and truecolor half-blocks otherwise,
never narrower than 16 columns. Nothing is drawn when the output is not a
terminal: `prometheus chat -z "hi" > out.txt` writes no escape sequence.
The sprite is removed when the pet is turned off or the interface ends.

The pet follows one mapping on every surface, in this order: a failure
shows the "failed" row, a completed plan "jump", an approval or question
waiting for you "waiting", a running tool "run", reasoning "review", any
other running turn "run", a turn that just finished "wave", and nothing
happening "idle". A message
of thanks or affection (`thanks`, `good bot`, `<3`, a heart emoji...)
plays a heart, detected locally; the message is sent as typed.

| Command | What it does |
|---|---|
| `/pet` | Toggle the pet (adopting the first installed one) |
| `/pet list` | The pet picker (the TUI's overlay installs gallery pets too) |
| `/pet <slug>` | Adopt an installed pet |
| `/pet scale <factor>` | Resize it everywhere |
| `/pet off` | Hide it |
| `/hatch <description>` (`/generate-pet`) | Generate a pet |

They run in the client: no model call, no turn.

## Generating a pet

`/hatch a sleepy fox` (or `prometheus pets hatch`, or Generate in the
desktop's pet list) works in two paid steps, each shown with its
estimated cost and run only after you answer yes: up to four drafts from
the description (pick one by number, `remix <tweak>`, `retry` or
`cancel`; the drafts are saved under `<profile>/cache/pets/hatch/`), then
one animation row per state generated with the chosen draft as reference
image, packed into a valid atlas and installed like a gallery pet. It uses
`pets.image_provider`, else the first configured image backend whose model
takes reference images; without one it stops with `pet generation needs
an image backend with reference-image support; configure one with
'prometheus tools'` (exit 78) and spends nothing.

## On the desktop

The active pet floats in the window. Alt + mouse wheel resizes it (saved
as `pets.scale`); with `pets.roam` it wanders along the bottom while the
agent is idle. Shift + click pops it out into a transparent,
always-on-top window kept out of the taskbar and window switcher where the
platform allows (on macOS the app keeps its Dock icon). The pop-out pet
has no connection of its own; it shows a speech bubble (`working…`,
`thinking…`, `your turn`), a click opens a mini composer whose prompt goes
to the most recent session through the main window's connection without
bringing the app forward, a double-click shows or hides the main window,
a mail badge appears when a turn finished while the app was hidden, and it
remembers where you put it. `↩` puts it back into the window. `/pet list`
and the command palette's "Pets…" open the pet list.

## Achievements

`prometheus achievements` and the dashboard's `/achievements` page show
tiered badges (`copper`, `silver`, `gold`, `diamond`, `olympian`) for
tool-chain feats, debugging, streaks, skill and memory use, model and
provider variety and session timing, each with exactly what it counts.
Numbers are computed per session and cached under
`<profile>/cache/achievements/`, so a scan analyses only new or changed
sessions; the first scan runs in the background while the page shows a
scanning indicator. Unlocked tiers are kept in
`<profile>/achievements.json`. Nothing leaves the machine.

```console
$ prometheus achievements --unlocked
<...> sessions (<...> analysed now)
```

After each turn the daemon rescans in the background; a tier reached
then is announced to the attached clients as `achievement.unlocked`: the
TUI and the desktop show a toast (Esc or × dismisses it) and the pet
jumps; the line interface prints a line. The event never reaches a model
or a session. A profile scanned for the first time records the tiers it
already earned without a celebration.
