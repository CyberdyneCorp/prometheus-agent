# Configuration, secrets and profiles

Prometheus keeps everything it knows in one directory, the **home**. Inside
it, every **profile** is an island with its own configuration, secrets,
sessions, memory and skills. This guide covers where things live, how
settings differ from secrets, how to work with profiles, and how to back
the whole home up.

Every command below is run by the documentation tests against a fresh home,
in order, so what you read is what the binary does. Lines starting with `$`
are commands; the lines after a command are (part of) its output, with
`<...>` standing for text that varies.

## The home

The home is chosen once when a command starts:

1. `--home PATH` when given,
2. else the `PROMETHEUS_HOME` environment variable,
3. else `~/.prometheus` (macOS, Linux) or `%LOCALAPPDATA%\Prometheus`
   (Windows).

It is created on first use with mode `0700`, and looks like this:

```text
<home>/
├── active_profile        the profile used without -p (absent: default)
├── daemon.toml           machine-wide daemon settings
├── run/                  runtime records and the writer lock
├── logs/                 daemon, update and migration logs
├── runtimes/             managed runtimes
├── updates/              install and update receipts
├── kanban/               boards shared by every profile
├── backups/              whole-home backup archives
└── profiles/
    ├── default/          a regular profile, like any other
    │   ├── config.toml   settings (mode 0600)
    │   ├── secrets.toml  secrets, file backend (mode 0600)
    │   ├── grants.toml   permanent approvals
    │   ├── profile_id    the profile's stable id (state.db is bound to it)
    │   ├── state.db      sessions and memory
    │   ├── blobs/        large payloads, by SHA-256
    │   ├── quarantine/   damaged state.db files (see the sessions guide)
    │   ├── snapshots/    state snapshots
    │   └── skills/ memories/ cron/ themes/ plugins/ ...
    └── work/
```

There is no `config.toml` at the home root: even `default` is an ordinary
directory under `profiles/`.

`state.db` records the id in `profile_id` when it is created and refuses to
open under any other profile (`store.profile_mismatch`, exit 78), so a
database copied from `work` into `home` is never written to. Renaming a
profile keeps its id. Before a newer Prometheus upgrades the database it
copies it to `<profile>/backups/state/state-v<old>-<time>.db` (the newest 3
are kept). An older Prometheus opens a newer database read-only (writes fail
with `store.schema_too_new`, exit 78), unless the newer build recorded that
older builds can no longer read it: then it refuses to open it
(`store.schema_unreadable`, exit 1) and leaves it untouched.

## Configuration

Each profile has one typed `config.toml`. Its schema is the single source
of every default: unset keys use their default, and an unknown key or an
invalid value is an error (exit 78) that names the key, the line and the
allowed values. `prometheus config docs` prints every key.

```console
$ prometheus config path
<home>/profiles/default/config.toml
$ prometheus config set agent.max_iterations 200
$ prometheus config get agent.max_iterations
200
$ prometheus config explain agent.max_iterations
source: config.toml:<...>
$ prometheus config unset agent.max_iterations
$ prometheus config get agent.max_iterations
500
```

`--set KEY=VALUE` overrides a key for one invocation and is never saved:

```console
$ prometheus --set agent.max_iterations=3 config get agent.max_iterations
3
$ prometheus config validate
<...>config.toml is valid
```

Writes keep your comments and formatting, are atomic, and keep a backup of
the previous file under `<profile>/backups/config/`.
`prometheus config edit` opens the file in `$VISUAL` / `$EDITOR` and does
not save until the result is valid.

### Live reload

A running gateway daemon watches each served profile's `config.toml`. A
saved change is validated first: an invalid file is reported (in the
daemon's log, `prometheus daemon status` and `prometheus doctor`) and the
daemon keeps serving the previous configuration. A valid one applies by
each key's apply class (`config explain <key>` shows it): `live` keys at
once, running sessions included (budgets, `tool_output.max_chars`,
`files.read_max_chars`, approvals and the rest apply from the session's
next turn or tool call, over the session's own `--set` overrides),
`next_session` keys to sessions created afterwards (a running session
keeps what it started with), and `restart` keys after `prometheus daemon
restart` (`daemon status` lists them as pending).

### Settings your administrator manages

An administrator can manage settings for every user of a machine in
`/etc/prometheus/managed.toml` (`%ProgramData%\Prometheus\managed.toml`
on Windows). It uses the same keys as `config.toml`, its values win over
every profile's, and the keys it lists in `locked = [...]` cannot be
changed: `config set`, `config unset`, `config edit` and `--set` refuse
them with `config key '<key>' is managed by your administrator` (exit 77).
`managed-secrets.toml` beside it (mode 0600 or 0640, owned by root) holds
secrets that take precedence over every profile store and the environment;
`prometheus secrets set` and `unset` refuse those names. `prometheus config
show` starts with a header naming the managed file and every managed or
locked key, and `prometheus doctor config` reports both files and whether
they were loaded. An invalid managed file stops every command (exit 78)
until it is fixed.

## Secrets are not configuration

API keys and tokens never go into `config.toml`. A value that looks like a
credential is refused:

```console
$ prometheus config set providers.acme.base_url sk-live-123456   # exits 2
error: providers.acme.base_url: this looks like a credential; store it with: prometheus secrets set <NAME>
```

Store it in the profile's secrets store instead, and refer to it by name
from configuration keys ending in `_secret`, for example
`providers.acme.key_secret = "ACME_API_KEY"`. A secret value is read from a
hidden prompt or from stdin, never from the command line, so it does not end
up in your shell history:

```console
$ echo -n "$TOKEN" | prometheus secrets set TELEGRAM_BOT_TOKEN
$ prometheus secrets list
TELEGRAM_BOT_TOKEN  file
$ prometheus secrets get TELEGRAM_BOT_TOKEN
TELEGRAM_BOT_TOKEN  file
$ prometheus secrets unset TELEGRAM_BOT_TOKEN
```

Secret names match `^[A-Z][A-Z0-9_]{0,127}$`. `secrets list` shows names,
where each comes from and whether another source hides it, never values.
`prometheus secrets get NAME --reveal` prints the value only to a terminal;
with stdout redirected it refuses (exit 77).

### Where secrets come from

A profile resolves a secret from, in order:

1. its secrets store: `<profile>/secrets.toml` (mode `0600`), or the OS
   keychain with `secrets.backend = "keychain"` (service `prometheus`;
   macOS Keychain, Windows Credential Manager or the Secret Service on
   Linux and other Unix systems; Android has no keychain backend, so keep
   `file` there); `prometheus secrets migrate --to keychain` moves every
   secret and switches the backend;
2. only for the profile the process was started for: the environment
   variables `PMT_SECRET_<NAME>`, then the provider's own variable (for
   example `OPENAI_API_KEY`);
3. a helper command you name, on Linux, macOS and other POSIX hosts:

   ```toml
   [secrets.command]
   enabled = true
   argv = ["/usr/local/bin/fetch-secrets", "--profile", "work"]
   timeout_secs = 3
   override_existing = false
   ```

   It runs once per process start, without a shell and in its own process
   group, and prints `NAME=VALUE` lines (at most 1 MiB; a blank value
   counts as unset; the first line naming a secret wins). Its stderr is
   discarded and only its exit status is logged. A helper that fails, does
   not finish within `timeout_secs` (its whole process group is killed) or
   prints anything else contributes nothing and prints one warning
   (`helper command failed (...)`, `helper output was not a NAME=VALUE
   map` or `secrets.command.argv is empty`); startup goes on. With
   `override_existing = true` its values come before the store and the
   environment. `prometheus secrets list` shows them as `command`. Windows
   has no command source.

A gateway serving several profiles never hands one profile's launch
environment to another. The environment is read for secrets only: any other
`PMT_*` or `PROMETHEUS_*` variable is ignored with a warning naming the
config key that replaces it.

Store values are read at each use, so a key rotated with
`prometheus secrets set` reaches the next request of a running session; the
session command `/reload-secrets` drops cached values at once and lists the
names whose value changed. Environment values stay fixed for the life of
the process. A missing secret fails with `secret.missing` and the command
that sets it.

## Profiles

A profile name is lowercase letters, digits, `_` and `-` (at most 64
characters, starting with a letter or digit). `prometheus`, `pmt`, `test`,
`tmp`, `root`, `all`, `none`, `run`, `logs` and `tools` are reserved.

```console
$ prometheus profile create ../evil   # exits 2
error: invalid profile name '../evil'
hint: <...>
$ prometheus profile create work
$ prometheus profile create coder --clone-from default
$ prometheus profile list
  coder
* default
  work
```

`--clone-from` copies configuration, the identity file, memory, skills,
themes and plugins once; the profiles do not stay linked. Secrets are copied
only with `--clone-secrets`.

A command uses the profile given with `-p/--profile`, else the one recorded
by `prometheus profile use`, else `default`:

```console
$ prometheus profile use work
$ prometheus config path
<home>/profiles/work/config.toml
$ prometheus -p coder config path
<home>/profiles/coder/config.toml
$ prometheus profile use default
```

Profiles share nothing at runtime: a setting, secret or skill of `work` is
invisible to `coder`.

```console
$ prometheus -p work config set agent.max_iterations 50
$ prometheus -p coder config get agent.max_iterations
500
```

Rename and delete with:

```console
$ prometheus profile rename coder helper
$ prometheus profile show helper
name: helper<...>
$ prometheus profile delete helper --yes
```

Deleting asks you to type the profile's name unless `--yes` is given (and
exits 4 when there is no terminal to ask). `default` cannot be deleted, a
profile in use by a running daemon or chat is refused (exit 75), and a
tombstone in `profiles/.deleted/` keeps any still-running process from
recreating the deleted directory. `prometheus profile create` with the same
name brings it back as a new, empty profile.

### Creation options

`prometheus profile create` also takes:

- `--clone-all`: copy everything from the source profile (`--clone-from`,
  else the selected profile), plugins, cron job definitions and themes
  included, but never its sessions, `state.db`, logs, snapshots or backups;
- `--clone-channels`: with a clone, keep the messaging adapters
  (`[gateway.platforms.*]`) and pairing data. Without it a clone drops
  them, so two profiles never answer the same bot account;
- `--sync-imports`: with a clone, also copy the import sync registry;
- `--no-skills`: start without bundled skills;
- `--description <text>`: shown by `profile list` and `profile show`;
- `--no-alias`: skip the alias launcher (below).

```console
$ prometheus profile create notes --description "reading notes" --no-alias
$ prometheus profile show notes
description: reading notes
```

### Alias launchers

`prometheus profile alias <name> [--as <alias>]` writes a launcher
`<alias>` (default: the profile name) into `~/.local/bin`
(`%LOCALAPPDATA%\Prometheus\bin` on Windows) that runs `prometheus -p
<name> "$@"`, so `work chat` means `prometheus -p work chat`. A profile
created under another home (`--home` or `PROMETHEUS_HOME`) gets a launcher
that passes `--home <that home>` as well. Every new
profile gets one unless `--no-alias` is given. A reserved name, a
`prometheus` subcommand or an existing executable on `PATH` is refused
(exit 2 naming it); a launcher written earlier is rewritten.

### Export and import

`prometheus profile export <name> -o <file>.tar.zst` writes the profile's
configuration, identity file, memory, [projects](projects.md), skills,
themes, scripts and cron job definitions. It never holds secrets, `state.db`, blobs, logs or backups
unless asked: `--with-sessions` adds the sessions, and `--with-secrets`
adds the secrets encrypted with a key derived from a passphrase you type
at a hidden prompt (Argon2id, XChaCha20-Poly1305).
`prometheus profile import <file> [--as <name>]` verifies every member
against the archive's manifest and asks for the passphrase before it
creates the profile, so a damaged archive or a wrong passphrase changes
nothing.

## Backups and snapshots

`prometheus backup create` writes the whole home to
`<home>/backups/prometheus-<UTC time>.tar.zst`, with a manifest of every
file's size and SHA-256. Databases are copied with SQLite's online backup,
so a running daemon does not need to stop. Runtimes, runtime records, logs,
caches, lock files and earlier backups are left out. `--quick` keeps only
critical state (configuration, secrets, grants, `state.db` and its
`profile_id`, cron jobs,
pairing data), `--label` names the archive and `--keep N` (default 3) removes
older default-named backups.

```console
$ prometheus backup create --quick --label before-upgrade
<home>/backups/prometheus-<...>-before-upgrade.tar.zst
```

`prometheus backup restore ARCHIVE` checks every file against the manifest
before writing anything (exit 65 when one does not match), refuses while a
daemon or chat uses an affected profile (exit 75), asks before replacing
existing data (`--force` skips the question; without a terminal it exits 4),
warns when the backup is older than the sessions it replaces, and snapshots
the state it replaces first. `-p NAME` restores only that profile.

A **snapshot** is a quick copy of one profile's critical state under
`<profile>/snapshots/`. Prometheus takes them itself before an update
(`pre-update`), a restore (`pre-restore`) and a store migration
(`pre-migration`); you take `manual` ones:

```console
$ prometheus snapshots create --label known-good
snap_<...>
$ prometheus snapshots list
snap_<...>  manual  <...>  known-good
$ prometheus snapshots prune --keep 5
```

`prometheus snapshots restore ID` takes a `pre-restore` snapshot and then
puts the snapshot's files back. Manual snapshots beyond `snapshots.keep`
(default 20) are pruned automatically. Inside a session the same actions
are available as `/snapshot [create [label]|list|restore <id>|prune [n]]`.
