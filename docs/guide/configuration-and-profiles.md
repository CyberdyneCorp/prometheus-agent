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
are kept); an older Prometheus opens a newer database read-only.

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
   keychain with `secrets.backend = "keychain"` (service `prometheus`);
   `prometheus secrets migrate --to keychain` moves every secret and
   switches the backend;
2. only for the profile the process was started for: the environment
   variables `PMT_SECRET_<NAME>`, then the provider's own variable (for
   example `OPENAI_API_KEY`).

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
