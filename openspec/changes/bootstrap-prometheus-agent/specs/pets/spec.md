# Pets and Achievements

## Purpose
Pets are optional animated mascots that mirror what the agent is doing across the line interface, the TUI and the desktop app, and achievements are collectible badges computed from the user's own session history. The key decision is that both are purely presentational: they are off by default, never touch the prompt, toolset, memory or token use of any session, and treat downloaded or generated sprites as untrusted images validated before display.

## ADDED Requirements

### Requirement: Pets are cosmetic and cache-neutral
Pets SHALL be disabled by default (`pets.enabled = false`) and SHALL have no effect on any session's system prompt, toolset, memory snapshot, model requests or token use; pet state SHALL be derived only from events the client already receives, and enabling, disabling or changing a pet SHALL never rebuild a session.
**Milestone:** M3

#### Scenario: Selecting a pet mid-session
- **GIVEN** a running session with a cached system prompt
- **WHEN** the user selects a pet
- **THEN** the session's system prompt hash and tool list SHALL be unchanged

### Requirement: Pet storage and sprite format
Installed pets SHALL live per profile at `<profile>/pets/<slug>/` with a `pet.toml` (slug, name, author, source, sha256 of the atlas, license) and an atlas image in the petdex layout (8 columns by 9 rows of 192×208 pixel cells, one row per animation state); an atlas SHALL be accepted only as PNG or WebP of at most 8 MiB whose dimensions match the layout, decoded with bounded memory, and anything else SHALL be rejected with `invalid pet atlas: <reason>` without being installed.
**Milestone:** M3

#### Scenario: Wrong dimensions
- **WHEN** a downloaded atlas measures 1000×1000 pixels
- **THEN** installation SHALL fail with `invalid pet atlas: expected 1536x1872` and nothing SHALL be written under `pets/`

### Requirement: Pet gallery commands
`prometheus pets list [query] [--limit N] [--installed]` SHALL list gallery pets from `pets.gallery_url` (default the public petdex gallery) filtered by substring, or installed pets with `--installed`; `prometheus pets install <slug> [--select] [--force]` SHALL download over HTTPS, validate and record the atlas hash, refusing to overwrite an installed pet without `--force`; `prometheus pets select [slug]` (a picker when omitted), `prometheus pets off` and `prometheus pets remove <slug>` SHALL update `pets.slug`/`pets.enabled` through a validated config patch, and an unknown slug SHALL exit 66.
**Milestone:** M3

#### Scenario: Install and select
- **WHEN** the user runs `prometheus pets install boba --select`
- **THEN** `pets/boba/` SHALL exist, `pets.slug` SHALL be `boba` and `pets.enabled` SHALL be `true`

#### Scenario: Removing the active pet
- **GIVEN** `boba` is the active pet
- **WHEN** the user runs `prometheus pets remove boba`
- **THEN** the pet SHALL be deleted and `pets.enabled` SHALL become `false`

### Requirement: Pet configuration and scale
The `pets` section SHALL hold `enabled` (default false), `slug` (default empty = first installed), `render_mode` (`auto` default, `kitty`, `iterm`, `sixel`, `unicode`, `off`), `scale` (default 0.33 of the native 192×208 cell, clamped to 0.1–3.0), `unicode_cols` (default 0 = derived from `scale`), `roam` (default false) and `image_provider` (default empty = automatic); `prometheus pets scale <factor>` and `/pet scale <factor>` SHALL set `scale` for every surface, clamping out-of-range values and saying so.
**Milestone:** M3

#### Scenario: Out-of-range scale
- **WHEN** the user runs `prometheus pets scale 5`
- **THEN** `pets.scale` SHALL be `3.0` and the command SHALL print `scale clamped to 3.0`

### Requirement: One activity-to-state mapping
Every surface SHALL map agent activity to pet states with one shared table evaluated in this precedence: a tool or turn just failed → `failed`; all todos of a plan completed → `jump`; an approval or clarify request is waiting for the user → `waiting` (`idle` for atlases without that row); a tool is executing → `run`; the model is streaming reasoning or reading → `review`; a turn is in flight otherwise → `run`; a turn just finished cleanly → `wave`; nothing is happening → `idle`.
**Milestone:** M3

#### Scenario: Approval pending
- **GIVEN** a turn waiting on an approval card
- **WHEN** the pet state is computed
- **THEN** the state SHALL be `waiting` on every surface showing the pet

### Requirement: Terminal pet rendering
The line interface and the TUI SHALL render the active pet only on a TTY, using the kitty, iTerm2 or sixel graphics protocol when `render_mode = "auto"` detects one and truecolor Unicode half-blocks otherwise (with a minimum legible width regardless of `scale`), SHALL never write graphics escape sequences to a pipe or file, and SHALL remove the sprite cleanly on exit or when the pet is turned off.
**Milestone:** M3

#### Scenario: Output redirected
- **GIVEN** an enabled pet
- **WHEN** the user runs `prometheus chat -z "hi" > out.txt`
- **THEN** `out.txt` SHALL contain no graphics escape sequences

### Requirement: Pet preview and doctor
`prometheus pets show [slug] [--state <s>] [--cycle] [--once] [--mode <m>] [--scale <f>]` SHALL animate a pet in the terminal until `Ctrl+C` (or once with `--once`), and `prometheus pets doctor` SHALL report the pets directory, installed pets, `pets.enabled`, `pets.slug`, the resolved active pet, the configured and detected render mode and the effective mode for this terminal, printing `ready` and exiting 0 only when a valid pet is installed, selected and enabled, else exiting 1 with the first missing item.
**Milestone:** M3

#### Scenario: Selected but disabled
- **GIVEN** `pets.slug = "boba"` and `pets.enabled = false`
- **WHEN** the user runs `prometheus pets doctor`
- **THEN** it SHALL exit 1 naming `pets.enabled is false`

### Requirement: Pet slash command
`/pet` SHALL toggle the pet (adopting the first installed pet when none is selected), `/pet list` SHALL open the gallery picker (an overlay in the TUI, the palette's pet list on desktop), `/pet scale <factor>` SHALL resize it, `/pet <slug>` SHALL adopt an installed pet and `/pet off` SHALL disable it, all client-side without a model call and without touching the session.
**Milestone:** M3

#### Scenario: Adopt from the TUI
- **GIVEN** pets `boba` and `mochi` installed
- **WHEN** the user types `/pet mochi`
- **THEN** `pets.slug` SHALL become `mochi` and no turn SHALL start

### Requirement: Generating a pet
`/hatch <description>` (alias `/generate-pet`) and the desktop generate flow SHALL create a pet in two steps: first at most 4 base drafts per round from the description, of which the user picks one, remixes or retries; then one animation row per state generated with the chosen draft as reference image and packed into a valid atlas; generation SHALL use `pets.image_provider` or else the first configured image backend that supports reference images, SHALL show the estimated cost and ask for confirmation before each paid step, and SHALL exit 78 with `pet generation needs an image backend with reference-image support; configure one with 'prometheus tools'` when none is available.
**Milestone:** M3

#### Scenario: No reference-capable backend
- **GIVEN** only a text-to-image backend without reference images is configured
- **WHEN** the user runs `/hatch a sleepy fox`
- **THEN** the command SHALL print the backend error and spend nothing

#### Scenario: Hatched pet is a normal pet
- **WHEN** generation completes
- **THEN** the result SHALL be installed under `pets/<slug>/` with a valid atlas and selectable like a gallery pet

### Requirement: Affection reactions
When a user message contains affection or gratitude aimed at the agent from a fixed local lexicon (for example "good bot", "thank you", "<3", heart emoji), every surface showing the pet SHALL play a heart reaction, detected locally without a model call and without altering the message.
**Milestone:** M3

#### Scenario: Thank-you message
- **WHEN** the user sends `thanks, good bot`
- **THEN** the pet SHALL show the heart reaction and the submitted message SHALL be unchanged

### Requirement: Desktop pet and pop-out overlay
The desktop SHALL draw the active pet as a floating sprite inside the window, resizable with `Alt` plus mouse wheel (persisted as `pets.scale`), wandering while the agent is idle when `pets.roam = true`, and SHALL let `Shift`-click pop it out into a transparent always-on-top overlay window that is absent from the dock and app switcher, carries no protocol connection of its own, persists its position, shows a speech bubble (`working…`, `thinking…`, `your turn`), opens a mini composer for the most recent session on click, toggles the main window on double-click and shows a mail badge when a turn finished while the app was hidden.
**Milestone:** M3

#### Scenario: Prompt from the overlay
- **GIVEN** the pet popped out and the app minimised
- **WHEN** the user clicks the pet and submits `check the build`
- **THEN** the prompt SHALL be submitted to the most recent session through the main window's connection without restoring the app

### Requirement: Achievements from session history
`prometheus achievements` and the dashboard `/achievements` page SHALL show tiered badges (`copper`, `silver`, `gold`, `diamond`, `olympian`) computed locally from the profile's session history (tool-chain feats, debugging patterns, streaks, skill and memory use, model and provider variety, session timing), each with a description of exactly what it counts; per-session statistics SHALL be cached by `(session_id, started_at, last_active)` so only new or changed sessions are re-analysed, the first scan SHALL run in the background without blocking the page, unlock state SHALL persist in `<profile>/achievements.json`, and no achievement data SHALL leave the machine.
**Milestone:** M3

#### Scenario: Incremental rescan
- **GIVEN** 2000 analysed sessions and 3 new ones
- **WHEN** the achievements page loads
- **THEN** only the 3 new sessions SHALL be analysed

#### Scenario: First scan does not block
- **GIVEN** a profile never scanned
- **WHEN** the user opens `/achievements`
- **THEN** the page SHALL render immediately with a scanning indicator and fill in when the scan finishes

### Requirement: Achievement unlocks celebrate (upgrade)
When an achievement tier unlocks, attached clients SHALL receive an `achievement.unlocked {profile, id, tier}` event, the desktop and TUI SHALL show a dismissible toast, and the active pet SHALL play `jump`; unlock events SHALL never be sent to the model or alter any session.
**Milestone:** M3

#### Scenario: Unlock during a session
- **WHEN** a finished turn completes the requirement of `night-owl` silver
- **THEN** the TUI SHALL show a toast and the pet SHALL jump, and the next model request SHALL contain no mention of it
