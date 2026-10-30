# Internationalization

## Purpose
Internationalization localizes Prometheus' static UI text (CLI prompts, approval prompts, gateway replies and notices, tool progress verbs, tips, TUI and desktop/dashboard strings) through layered string catalogs selected by `display.language`. Languages are pluggable: bundled catalogs, per-profile overlays and no-code language-pack plugins can add or override any key. The key decisions are that agent replies, tool output, logs and command names are never translated, and that localized text never enters the model context, so changing the language can never break the prompt cache.

## ADDED Requirements

### Requirement: Static UI string catalogs
The system SHALL render every user-facing static UI string on the CLI, gateway, TUI, desktop and dashboard through a keyed catalog lookup with dotted keys grouped by area (`approval.*`, `gateway.*`, `cli.*`, `display.*`, `slash.*`, `tips.*`, `tui.*`, `desktop.*`), with three catalog surfaces `core`, `tui` and `desktop`; agent responses, tool output, logs, error codes, config keys and slash-command names SHALL NOT be translated, and localized strings SHALL NOT be inserted into any model request.
**Milestone:** M2

#### Scenario: Language change keeps the prompt cache
- **GIVEN** an active session in language `en`
- **WHEN** the user sets `display.language = "de"` and sends the next message
- **THEN** the approval prompts SHALL appear in German and the system prompt and tool definitions sent to the model SHALL be byte-identical to the previous call

### Requirement: Bundled languages
The release SHALL bundle complete `core` and `tui` catalogs for `en`, `af`, `ar`, `de`, `es`, `fr`, `ga`, `hu`, `it`, `ja`, `ko`, `pt`, `ru`, `tr`, `uk`, `zh` and `zh-hant`, with `en` the source of truth whose key sets are exported to `schema/locales/keys.<surface>.json`, and CI SHALL fail when a bundled catalog contains a key missing from English or a placeholder set differing from English.
**Milestone:** M2

#### Scenario: Bundled language list
- **WHEN** the user runs `prometheus config set display.language xx`
- **THEN** the error SHALL list at least the 17 bundled ids

### Requirement: display.language selection
The system SHALL select the UI language from `display.language` (default `en`, lowercase BCP-47-style id matching `^[a-z]{2,3}(-[a-z0-9]{2,8})*$`) per profile, SHALL refuse a value no catalog layer supplies with exit 78 and `Unknown language '<id>' for display.language. Available: <ids>` plus the hint to install a language pack or add `<profile_home>/locales/<id>.toml`, and SHALL NOT read any environment variable for the language; a gateway serving several profiles SHALL render each profile's messages in that profile's language.
**Milestone:** M2

#### Scenario: Unknown id refused
- **WHEN** the user runs `prometheus config set display.language pl` with no Polish catalog installed
- **THEN** the command SHALL exit 78 naming the available ids and the config SHALL be unchanged

#### Scenario: Two profiles, two languages
- **GIVEN** profiles `a` with `display.language = "es"` and `b` with `"ja"` on one gateway
- **WHEN** both receive a `/status` reply
- **THEN** `a`'s reply SHALL use Spanish strings and `b`'s Japanese strings

### Requirement: Layered lookup with partial catalogs
For each key the system SHALL take the first hit from: enabled language-pack plugins declaring the language (the last loaded wins when two cover the same key), then the profile overlay `<profile_home>/locales/<lang>[.tui|.desktop].toml`, then the bundled catalog; then the same three layers for `en`; then the raw key; every layer MAY be partial, and an overlay for a language with no bundled catalog SHALL make that language selectable.
**Milestone:** M2

#### Scenario: Overlay changes one string
- **GIVEN** `<profile_home>/locales/en.toml` containing only `[approval] denied = "Nope."`
- **WHEN** an approval is denied
- **THEN** the denial text SHALL be `Nope.` and every other string SHALL be the bundled English

### Requirement: Placeholder and value validation
Catalog values SHALL be strings only; core strings SHALL use the same named `{placeholder}` set as the English value and TUI/desktop function-valued strings SHALL use positional `{0}`, `{1}` placeholders; at load, a non-string value SHALL make the whole file invalid (reported, file ignored), and a value whose placeholder set differs from English SHALL fall back to the English string for that key with one WARN per key.
**Milestone:** M2

#### Scenario: Misspelled placeholder falls back
- **GIVEN** a German overlay value `"{cuont} Agenten"` for a key whose English uses `{count}`
- **WHEN** the string is rendered
- **THEN** the English string SHALL be shown with the count substituted and a WARN SHALL name the key

### Requirement: Language-pack plugins
A plugin of any kind MAY declare `[[provides_locales]]` entries (`id`, `endonym` default the id, `rtl` default false) and ship `locales/<id>.toml`, `locales/<id>.tui.toml` and `locales/<id>.desktop.toml`; a plugin whose only contributions are locales SHALL need no code (a `wasm` manifest with an empty `worlds` list instantiates no component) and no capabilities; packs SHALL be installed, signed, enabled and catalog-resolved like any plugin, SHALL be registered automatically from the declared files, SHALL be unregistered when the plugin is disabled, and SHALL never change `display.language` themselves; a Hermes language pack (`plugin.yaml` with `provides_locales` and YAML catalogs, or `ctx.register_locale`/`register_locale_dir` through the python bridge) SHALL be converted to TOML catalogs at install time with text-only validation.
**Milestone:** M2

#### Scenario: No-code pack adds a language
- **GIVEN** an installed and enabled pack declaring `id = "pl"`, `endonym = "Polski"` with `locales/pl.toml`
- **WHEN** the user runs `prometheus config set display.language pl`
- **THEN** the command SHALL succeed and CLI approval prompts SHALL use the pack's Polish strings

### Requirement: Language pack validation
`prometheus plugins validate <path>` SHALL check for every declared locale that `locales/<id>.toml` exists, that each catalog parses and contains only string values (error, exit 65), that placeholders match English (error), and SHALL warn, listing them, about keys absent from the English key export for that surface.
**Milestone:** M2

#### Scenario: Unknown key warned not failed
- **GIVEN** a pack catalog with key `approval.denyed`
- **WHEN** it is validated
- **THEN** the report SHALL warn `unknown key approval.denyed` and the command SHALL exit 0

### Requirement: Language switchers show endonyms
The TUI language picker, the desktop settings and the dashboard SHALL list every selectable language (bundled, overlay and installed packs) by its endonym with its id, SHALL write the choice to `display.language` through the config RPC, and the desktop and dashboard SHALL mirror their layout when the selected language is declared `rtl` (bundled `ar` is RTL); a surface lacking a catalog for the selected language SHALL render English for missing keys.
**Milestone:** M3

#### Scenario: RTL layout
- **WHEN** the user selects `ar` in desktop settings
- **THEN** the desktop layout SHALL switch to right-to-left without restarting the app

### Requirement: Language changes apply without restart (upgrade)
A change to `display.language`, an overlay file or an enabled language pack SHALL take effect for UI text rendered after the change on every running surface of that profile within 5 seconds, without restarting the daemon, TUI or desktop, and SHALL never rebuild any session's prompt or tool list.
**Milestone:** M2

#### Scenario: Gateway picks up an overlay edit
- **GIVEN** a running gateway
- **WHEN** the user edits `<profile_home>/locales/en.toml`
- **THEN** the next gateway notice SHALL use the edited text without a restart

### Requirement: Migration of Hermes language settings
`hermes-migration` SHALL map Hermes `display.language` (and a `HERMES_LANGUAGE` entry in the Hermes `.env`) to `display.language`, convert each Hermes profile overlay `<hermes_home>/locales/<lang>[.tui|.desktop].yaml` into the matching TOML overlay, and report entries skipped because a value was not text.
**Milestone:** M2

#### Scenario: Env override becomes config
- **GIVEN** a Hermes home whose `.env` contains `HERMES_LANGUAGE=fr`
- **WHEN** `prometheus migrate from-hermes` runs
- **THEN** the profile's `config.toml` SHALL contain `display.language = "fr"` and no environment variable SHALL be needed
