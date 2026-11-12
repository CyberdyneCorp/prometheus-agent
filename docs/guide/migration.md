# Migrating from Hermes and other agents

`prometheus migrate` brings what you built in another agent into a
Prometheus profile: from Hermes Agent everything (configuration, secrets,
identity, memory, skills, sessions, scheduled jobs, MCP servers, plugins and
messaging settings), and from OpenClaw, Claude Code, Codex CLI and Cursor
their instructions, memory, skills, MCP servers and settings.

Every importer follows the same rules:

- **The source is read only.** Nothing under `~/.hermes`, `~/.claude`,
  `~/.codex`, `~/.cursor` or `~/.openclaw` is created, changed, locked or
  deleted. Hermes' `state.db` is copied into a staging directory of the
  profile and only the copy is opened, so a running Hermes is never blocked
  (a warning says that changes made after the snapshot come with a re-run).
- **Every item gets one disposition** in the report: `mapped`,
  `transformed`, `default_omitted`, `secret`, `unsupported`, `ambiguous`,
  `conflict`, `skipped` or `failed`, with where it went and why. Reports
  name secrets but never contain their values or your messages; a real run
  saves the report as `<profile>/migrations/<source>-<run-id>.json`.
- **Nothing is written without asking.** A real run shows the plan and
  asks first; without a terminal it needs `--yes` and otherwise exits 4.
  `--dry-run` prints the plan and writes nothing at all (not even the
  Prometheus home).
- **Re-runs are safe.** A ledger per source
  (`<profile>/migrations/<source>-ledger.toml`) records what each run
  imported, so running the same command again imports only what is new or
  changed, never duplicates, and never overwrites a value you edited in
  Prometheus since: that item is reported `conflict` and kept, unless you
  pass `--overwrite`. What you removed in Prometheus stays removed while
  its source is unchanged: a revoked pairing, a deleted session, memory
  entry, skill or plugin, a dropped command approval.

## From Hermes

Look at the plan first. Without options the migration takes the Hermes
profile Hermes has active; `--all` takes every profile (each to the
Prometheus profile of the same name, the root profile to `default`) and
`--profile NAME` (repeatable) picks some. A Hermes name Prometheus
reserves needs `--into-profile`.

```console
$ prometheus migrate from-hermes --dry-run
Plan from <...>/.hermes
profile default -> default
secrets 1 secret
memory 1 mapped
sessions 1 mapped
$ prometheus migrate from-hermes # exits 4
<...>refusing to migrate non-interactively without --yes<...>
```

Then migrate. Each component is committed on its own and recorded in the
ledger, in this order: secrets, config, identity, memory, skills,
sessions, cron, mcp, plugins, gateway. A component that fails is undone
(none of its writes stay) while the committed ones are kept; sessions
commit in batches of 500, so a re-run continues after the last committed
batch. Choose components with
`--only` and `--skip`:

```console
$ prometheus migrate from-hermes --yes
Migrated from <...>/.hermes
sessions 1 mapped
$ prometheus migrate from-hermes --only sesions # exits 2
<...>unknown component 'sesions'; valid components: config, secrets, identity, memory, skills, sessions, cron, mcp, plugins, gateway<...>
```

What lands where:

- `config.yaml` keys you set (never Hermes defaults) become `config.toml`
  keys; an ambiguous YAML value such as an unquoted `off` takes the more
  restrictive reading and is reported `ambiguous`.
- `.env` entries are sorted: secrets go to the secrets store, settings to
  `config.toml`. Credential files (OAuth sign-ins, credential pools) are
  imported as sign-ins and secrets.
- `SOUL.md` and the selected personality become the profile's `SOUL.md`;
  `memories/MEMORY.md` and `USER.md` become memory entries in their order.
- The UI language (`display.language`, or `HERMES_LANGUAGE` in `.env`)
  becomes `display.language`, and the profile's `locales/*.yaml` overlays
  become TOML overlays (the identity component; see
  [UI languages](languages.md)).
- Your skills are copied under the Prometheus skill scan: a skill the scan
  flags goes to `skills/.quarantine/` and is disabled. Unchanged Hermes
  bundled skills stay behind.
- Sessions keep their lineage, usage and Hermes ids: `prometheus chat
  --resume <hermes id>` resumes the imported session.
- Scheduled jobs arrive **paused** so they never fire from both agents
  (`--enable-cron` keeps enabled ones enabled; stop the Hermes scheduler
  first). Plugins and messaging adapters arrive disabled; Telegram and
  other users you approved through pairing stay approved.

### Running it again

Run the same command whenever Hermes has new work. Items unchanged since
the last run are reported `skipped` ("unchanged since the last
migration"), new sessions are imported, and a Hermes session that grew is
appended to its Prometheus copy unless you continued it here:

```console
$ prometheus migrate from-hermes --yes
sessions 1 skipped
```

If you press Ctrl+C, the run stops at the next commit (between components
or after a batch of 500 sessions) and exits 130; run the same command to
continue where it stopped. Exit codes: 0 when every selected component
committed (unsupported items and conflicts do not fail a run), 1 when a
component failed, 66 when the Hermes home is missing, 73 when the
Prometheus profile cannot be written.

## From OpenClaw, Claude Code, Codex CLI and Cursor

See what is on this machine and what each source would import:

```console
$ prometheus migrate detect
openclaw <...>
claude-code <...>
```

Each importer reads its usual directory (`--source PATH` for another one)
and writes into the active profile (`--profile NAME` for another). A real
run first takes a `pre-migration` snapshot of the profile (`prometheus
snapshots list`; skip it with `--no-backup`).

```console
$ prometheus migrate from-claude-code --dry-run
<...>memory mapped CLAUDE.md -> agent memory (1 entries)
<...>config transformed settings.json:permissions.allow:Bash(npm run test:*) -> grants.toml<...>
<...>config unsupported settings.json:permissions.allow:WebFetch<...>
$ prometheus migrate from-claude-code --yes
Migrated from <...>/.claude
```

| Source | Imported |
|---|---|
| Claude Code (`~/.claude`) | `CLAUDE.md` paragraphs to memory; `skills/` to `skills/claude-code-imports/`; MCP servers from `~/.claude.json` and `settings.json`; `Bash(...)` rules of `permissions.allow` to permanent command approvals in `grants.toml` (`Bash(npm run test:*)` becomes `npm run test*`) and of `permissions.deny` to `approvals.deny`. Other permission rules and `commands/*.md` are reported unsupported (turn a command into a skill). |
| Codex CLI (`~/.codex`) | `AGENTS.md` and `memories/*.md` to memory; `skills/` to `skills/codex-imports/`; `[mcp_servers.*]` of `config.toml`. |
| Cursor (`~/.cursor`) | `AGENTS.md` and `rules/*.md`, `*.mdc` (front matter dropped) to memory; skills at any depth under `skills/` to `skills/cursor-imports/`; `mcp.json`. |
| OpenClaw (`~/.openclaw`, `~/.clawdbot`, `~/.moltbot`) | Workspace `SOUL.md` to `SOUL.md`; `MEMORY.md`, `USER.md` and `memory/*.md` to memory; skills to `skills/openclaw-imports/`; MCP servers; the time zone; with `--preset full` also models and providers, reasoning effort, turn limit, compaction, exec timeout and the Docker sandbox. |

Memory entries are deduplicated against what the profile already holds and
carry their provenance (`import:claude-code`). A malformed file (a
`settings.json` that is not JSON) is reported `failed` and the command exits
1, but everything else still imports.

**Other agents never hand over secrets.** Credential files
(`~/.claude/.credentials.json`, `~/.codex/auth.json`,
`~/.cursor/cli-config.json`, OpenClaw's auth stores) are never read. An MCP
environment variable or header that looks like a secret (`*_TOKEN`,
`*_KEY`, `*_SECRET`, `*PASSWORD*`, `Authorization`) is stripped: the server
refers to the secret by name and the report tells you to add it with
`prometheus secrets set <NAME>`. Only OpenClaw can carry secret values, and
only with `--migrate-secrets`, into the secrets store:

```console
$ prometheus migrate from-openclaw --preset full --yes
Migrated from <...>/.openclaw
$ prometheus config get agent.reasoning_effort
medium
```

OpenClaw's `--skill-conflict skip|overwrite|rename` decides what a skill
that already exists in the profile does (`rename` imports it as
`<name>-imported`), and `--workspace-target PATH` copies the workspace
`AGENTS.md` into a project.

### Keeping imports in sync

A successful Claude Code, Codex CLI or Cursor import registers itself in
`<profile>/migrations/import-sync.toml`. `prometheus migrate sync`
re-imports every registered source whose files changed, without asking, so
it can run as a scheduled job. A changed skill is refreshed unless you edited
the imported copy (then it is a `conflict`), and a token refresh never
triggers a sync because credential files are not part of what is compared
(of `~/.claude.json`, which Claude Code rewrites on every launch, only
`mcpServers` is compared). Memory entries and command approvals the sync
imported before are not added back once you removed them:

```console
$ prometheus migrate sync
claude-code <...> skipped: unchanged
```

A profile cloned with `prometheus profile create --clone-from` carries the
registry only with `--sync-imports`.

### Archiving OpenClaw

Once you have moved to Prometheus, archive OpenClaw's directories so the two
agents never act on the same state. Nothing is deleted: each directory is
renamed to `<name>.pre-migration`, and the command refuses (exit 75) while
OpenClaw is running. No importer does this on its own.

```console
$ prometheus migrate cleanup openclaw --yes
renamed <...>/.openclaw to <...>/.openclaw.pre-migration
```

## Conversations from Claude Code and Codex CLI

`prometheus migrate sessions` imports one conversation as a session titled
`Imported from Claude Code: <first message>` (or `Codex CLI`). The user and
assistant messages come over in order; tool calls become `[ran tool: <name>]`
notes, and system prompts, injected context, reasoning and raw tool output
stay out. Without a file, a picker lists the logs newest first with a
preview. Importing the same log again returns the session it created:

```console
$ prometheus migrate sessions --from codex ~/.codex/sessions/2026/11/01/rollout-2026-11-01T10-00-00-guide.jsonl
imported <...>
resume it with: prometheus chat --resume ses_<...>
$ prometheus migrate sessions --from codex ~/.codex/sessions/2026/11/01/rollout-2026-11-01T10-00-00-guide.jsonl
already imported as <...>
```
