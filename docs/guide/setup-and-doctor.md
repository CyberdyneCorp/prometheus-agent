# Setup, onboarding and doctor

## The setup wizard

`prometheus setup [model|secrets|tools|terminal|memory|gateway]` walks
through a profile's settings. Each question shows the current value and
Enter keeps it, so running the wizard again and accepting every default
leaves `config.toml` byte-identical. Values are written only through the
config writer; API keys go to the secrets store (`secrets.toml` or the
keychain), never to `config.toml`.

- `model`: pick a provider and model, or the ChatGPT/Codex subscription
  sign-in (the provider becomes `model.provider`); a provider that needs
  an API key asks for it (hidden on a terminal) when none is stored.
- `secrets`: the secrets the configured providers read, `set` or
  `missing`, asking for the missing ones.
- `tools`: the tool configurator (below).
- `terminal`: `terminal.backend` (`local` in this build).
- `memory`: `memory.memory_enabled` and `memory.user_profile_enabled`.
- `gateway`: messaging adapters, available with the gateway (exit 69
  until then).

Without a section the wizard runs model, secrets, terminal and memory.
An interactive `prometheus` on a profile with no model offers `setup
model` first. `--non-interactive` writes the `--set` values instead of
asking and takes missing secrets from the environment (`PMT_SECRET_<NAME>`
or the provider's own variable) into the secrets store; a missing model
or secret exits 78 naming it:

```text
prometheus --set model.provider=anthropic --set model.default=claude-sonnet-4-5 \
  setup model --non-interactive
```

## Onboarding

First-run progress (setup offered, one-time tips, tour steps) is kept in
the profile's state store, not `config.toml`. Each tip shows once per
profile; `prometheus setup --reset-onboarding` clears the record.

## The tool configurator

`prometheus tools configure [--surface cli|tui|cron|acp|gateway.<platform>]`
toggles toolsets (saved in `toolsets.surfaces.<surface>`, or
`tools.enabled_toolsets` / `tools.disabled_toolsets` without a surface)
and picks a provider per tool category. The non-interactive forms are
`prometheus tools enable|disable <toolset> [--surface S]` and
`prometheus tools provider <category> <provider>`. Categories are
web_search (`web.search_backend`: `auto`, `firecrawl`, `searxng`),
web_extract (`web.extract_backend`: `auto`, `firecrawl`), browser
(`browser.provider`: `local`), and image_generation, tts, stt, video and
x_search, which have no provider in this build. Unknown
toolsets, surfaces, categories or providers exit 78 listing the valid
names and write nothing; posture toolsets such as `coding` are never
saved. Changes apply from the next session.

```console
$ prometheus tools provider web_search nosuch # exits 78
error: unknown web_search provider 'nosuch'; registered: auto, firecrawl, searxng
$ prometheus tools provider web_search searxng
web.search_backend = "searxng"; applies from the next session
$ prometheus tools enable web --surface cron
toolset web enabled for cron; applies from the next session
```

## Setup summary for support

`prometheus debug summary` prints a plain-text block to paste into a
support thread: version line, OS and architecture, install owner, profile
and home, model and provider, terminal backend and sandbox enforcement,
secret names (`set` or `missing`), enabled toolsets, MCP server count,
memory provider, daemon state, scheduled job and skill counts, and every
config key whose value differs from the default. Credentials inside URLs
become `***` and secret values never appear; `--show-key-prefixes` shows
the first and last four characters of secrets longer than 16 characters.

## Doctor

`prometheus doctor [<group>] [--fix] [--online] [--strict] [--json]
[--all-profiles]` prints one `ok`, `warn` or `fail` line per check with a
hint, and exits 1 when any check fails (`--strict`: any warning too).
Groups: `home` (0700 directories, 0600 files), `config` (validity,
pending schema migration, ignored environment variables), `secrets`
(store mode, referenced-but-missing secrets), `store` (opens `state.db`,
schema version, full-text indexes, quarantine records), `daemon`,
`sandbox` (mechanism and strength), `terminal` (backend and shell),
`runtimes`, `plugins`, `observability`, `network` (IPv6 route, egress),
`providers` (credentials; reachability only with `--online`) and
`security` (trust boundaries, sandbox enforcement of `fs`, `net`,
`process` and `syscalls`, downgrades, redaction). Without `--online`
doctor makes no network connection. `prometheus doctor security --strict`
exits 77 when anything is downgraded. `--json` prints one
`prometheus.doctor.v1` document.

```console
$ prometheus doctor nosuch # exits 2
error: unknown doctor group '<name>'; groups: <groups>
$ prometheus doctor store
ok store state.db opens at schema version <n>
$ prometheus setup --reset-onboarding
onboarding reset (<n> records cleared)
```

`--fix` performs only these repairs and prints each one: tighten home,
secrets and database permissions; remove stale `run/` lock and record
files whose pid is gone (`removed stale gateway lock (pid <n> not
running)`); rebuild the full-text index; apply a pending config schema
migration after copying `config.toml` to `config.toml.bak.<timestamp>`;
reinstall managed runtimes whose hash does not match (none are installed
by this build). Everything else is reported with a hint and left as it
is. Note that every command tightens loose permissions of the home and
the profile's `config.toml` and `secrets.toml` when it opens them
(logged as a WARN); doctor reports what it found before that happens.
