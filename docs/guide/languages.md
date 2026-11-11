# UI languages

Prometheus shows its own interface text in the language
`display.language` selects, per profile: the interactive CLI (prompts,
notices, tool progress lines, the turn summary, one-time tips), the
`/help` list and the replies of slash commands on every surface, chat
notices, cards and buttons on messaging platforms, and the TUI (status
bar, session switcher, command palette and its own commands' help,
cards). Only that static text is translated: the agent's replies, tool
output, logs, error messages and codes, config keys and slash-command
names are not, nor are the reports of non-interactive `prometheus
<command>` runs (scripts read them). Localized text never enters a model
request, so changing the language never breaks a session's prompt cache.

Not yet translated: replies a subsystem renders itself (`/insights`,
`/context`, `/toolsets`, `/skills`, `/bundles`, `/reload-skills`,
`/memory`, `/journey`, `/goal`, `/subgoal`, `/loop`, `/heartbeat`,
`/egress`, `/curator`, `/debug`, the `/suggestions` list, the
`/personality` list, the `/model` switch notice) and the notices the agent
adds to a turn (fast-mode tier, budget warnings, context pressure,
guardrail stops, scheduled-job failures) still show in English.

## Choosing a language

```console
$ prometheus config set display.language de
$ prometheus config get display.language
de
$ prometheus config set display.language pl # exits 78
error: Unknown language 'pl' for display.language. Available: af, ar, de, en, es, fr, ga, hu, it, ja, ko, pt, ru, tr, uk, zh, zh-hant
hint: install a language pack plugin that provides 'pl', or add <path>
$ prometheus config set display.language en
```

The bundled languages are `en` (the default), `af`, `ar` (right to left),
`de`, `es`, `fr`, `ga`, `hu`, `it`, `ja`, `ko`, `pt`, `ru`, `tr`, `uk`, `zh`
and `zh-hant`. Ids are lower case (`^[a-z]{2,3}(-[a-z0-9]{2,8})*$`). No
environment variable sets the language. A gateway serving several profiles
answers each in that profile's language.

A change applies to text shown after it on every running surface of the
profile within a few seconds, without restarting the daemon, the TUI or a
chat: a language change, an overlay edit, or enabling or disabling a
language pack.

## Changing single strings: overlays

Each string has a dotted key (`approval.denied`, `gateway.nothing_to_stop`,
`tui.approval_hint`). A profile overlay replaces any of them:
`<profile_home>/locales/<lang>.toml` for the CLI and gateway strings,
`<lang>.tui.toml` for the TUI and `<lang>.desktop.toml` for the desktop
app. For example, `<profile_home>/locales/en.toml`:

```toml
[approval]
denied = "Nope."
```

Every catalog may be partial. For each key the first of these that has it
wins: the enabled language packs that declare the language (the last loaded
first), the profile overlay, the bundled catalog; then the same three for
English; then the key itself. An overlay for a language nothing bundles
(`locales/eo.toml`) makes that language selectable.

Values are text only, and keep the English placeholders: named ones such
as `{tool}` in CLI and gateway strings, positional `{0}`, `{1}` in TUI and
desktop strings. A file with a value that is not text is ignored as a
whole (logged); a value whose placeholders differ from English falls back
to the English text for that key, with one warning naming it. The English
key sets are in `schema/locales/keys.core.json`, `keys.tui.json` and
`keys.desktop.json`.

## Language packs

A language pack is a plugin that ships catalogs; it needs no code. Its
`plugin.toml` is a `wasm` manifest with an empty `worlds` list (no
component is needed or loaded) that declares its languages:

```toml
manifest_version = 1
id = "polski"
version = "1.0.0"
kind = "wasm"
description = "Polish UI strings."
license = "MIT"
requires_prometheus = ">=0.1"
worlds = []

[[provides_locales]]
id = "pl"
endonym = "Polski"   # default: the id
rtl = false          # default
```

with `locales/pl.toml` (and optionally `locales/pl.tui.toml` and
`locales/pl.desktop.toml`) beside it. A pack is installed, signed, enabled
and resolved like any plugin; its languages become selectable while it is
enabled and stop being selectable when it is disabled. It never changes
`display.language` itself.

`prometheus plugins validate <dir>` checks a pack: each declared language
has `locales/<id>.toml`, each catalog parses with text values only and the
English placeholders (errors, exit 65), and keys English does not have are
listed as warnings (`unknown key approval.denyed`) without failing.

A Hermes language pack (`plugin.yaml` with `provides_locales`, or a plugin
calling `ctx.register_locale` or `ctx.register_locale_dir`) is converted
when it is installed: its `locales/*.yaml` catalogs become TOML catalogs,
keeping text values only.

## Migrating from Hermes

`prometheus migrate from-hermes` maps Hermes `display.language` (and
`HERMES_LANGUAGE` in the Hermes `.env`, which wins) to `display.language`,
and converts each Hermes overlay `<hermes_home>/locales/<lang>[.tui|.desktop].yaml`
into the matching TOML overlay. Entries that are not text are skipped and
listed in the migration report.
