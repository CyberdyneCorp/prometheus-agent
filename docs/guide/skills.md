# Skills

A skill is the agent's procedural memory: a directory with a `SKILL.md`
(YAML frontmatter plus Markdown instructions) and optional supporting
files. The system prompt lists every available skill in one line each;
the model loads a skill's full text only when it needs it, with
[`skill_view`](../reference/tools.md#skill_view), or when you invoke it.

## Where skills come from

Skills are searched in one precedence order; the first tier that defines a
name wins and later copies are *shadowed*:

1. trusted project skills (`.prometheus/skills/`, `.agents/skills/` of a git
   root listed by `prometheus skills trust <root>`),
2. the profile's `skills/` directory,
3. each `skills.external_dirs` entry,
4. plugin skills,
5. the bundled skills shipped in the binary (read-only).

`prometheus skills list` shows every skill with its category, source and
status; `prometheus skills show <name>` shows one with its files and
`SKILL.md`.

```console
$ prometheus skills list
NAME              CATEGORY              SOURCE   STATUS
prometheus-agent  autonomous-ai-agents  bundled  enabled
$ prometheus skills show prometheus-agent
name: prometheus-agent
status: enabled
```

`prometheus-agent` is the essential skill: it teaches the agent to operate
Prometheus itself and cannot be disabled.

## Enabling, disabling and ejecting

```console
$ prometheus skills disable prometheus-agent   # exits 78
error: skills.disabled: the essential skill 'prometheus-agent' cannot be disabled
```

`prometheus skills disable <name>` hides a skill everywhere;
`--platform telegram` hides it on one surface only
(`skills.platform_disabled.telegram`). `prometheus skills enable <name>`
undoes either. The change applies from the next session; in a chat,
`/skills disable <name> --now` applies it at once at the cost of the prompt
cache.

Bundled skills are read-only. `prometheus skills eject <name>` copies one
into the profile, where you can edit it; an upgrade never touches the copy.
`prometheus skills list --outdated` names ejected skills whose bundled
version changed, `prometheus skills diff <name>` shows the difference and
`prometheus skills reset <name>` drops the copy.

### A profile without bundled skills

`prometheus profile create <name> --no-skills` (or `prometheus skills
opt-out` on an existing profile) sets `skills.include_bundled = false`: no
bundled skill appears in listings, the index or slash commands, and new
bundled skills of later releases do not appear either. `prometheus-agent`
stays. `prometheus skills opt-out --remove` also deletes ejected copies that
are still identical to their bundled origin (it asks first; `--yes` skips
the question); `prometheus skills list-modified` lists the ones you edited.
`prometheus skills opt-in` restores the default.

```console
$ prometheus skills opt-out
bundled skills are hidden from the next session (prometheus-agent stays)
$ prometheus skills opt-in
bundled skills are shown again from the next session
```

Official catalog skills installed into the profile are recorded in
`skills/skills.lock`. `prometheus skills repair-official <name>|all`
re-records the provenance of installs that still match the copy embedded in
the binary; `--restore` replaces missing or modified copies, moving the old
copy to `skills/.backups/` first.

## Installing skills from the hub

`prometheus skills install <identifier>` fetches a skill into a quarantine
directory, scans it, checks its signature and decides by the source's trust
level, then moves it into the profile and records it in
`skills/skills.lock`:

| Identifier | Source | Trust |
|---|---|---|
| `official/<category>/<name>` | the official catalog embedded in the binary (works offline) | `builtin` |
| `<owner>/<repo>/<path>`, `github:<owner>/<repo>/<path>` | a GitHub repository path or a tap | `trusted` when the repository is in `skills.trusted_sources`, else `community` |
| `skills-sh/<owner>/<repo>/<skill>` | the skills.sh directory (alias slugs resolve) | as GitHub |
| `well-known:<https url>` | a site's `/.well-known/skills/` endpoint | `community` |
| `clawhub:<slug>`, `lobehub:<slug>`, `browse-sh:<slug>` | the registries | `community` |
| `https://.../SKILL.md` | one file plus the support files it references, same host | `community` |

`builtin` allows every scan verdict, `trusted` allows `safe` and `caution`,
`community` only `safe`. `--force` overrides a block, except a `dangerous`
verdict on a `trusted` or `community` skill, which nothing overrides.
Reinstalling needs `--force`. A bare `SKILL.md` URL whose frontmatter and
slug give no name needs `--name`. GitHub requests use the `GITHUB_TOKEN`
secret when it is set.

```text
$ prometheus skills browse --source official   # offline
$ prometheus skills search terraform            # cached index first, then live sources (8 s at most)
$ prometheus skills inspect openai/skills/k8s   # preview, scan and upstream metadata; writes nothing
$ prometheus skills install openai/skills/k8s
```

A skill carrying a `blueprint` block is never scheduled by its install: its
blueprint appears in `prometheus cron suggestions` and `/suggestions` until
you accept it.

### Keeping hub skills honest

`skills.lock` records each hub skill's source, the resolved upstream ref (a
commit SHA, a version or a URL), the SHA-256 of its installed files, its
trust level and its signature state. When a session starts, a hub skill
whose files no longer match is left out of the index and quarantined:
`prometheus skills doctor` reports `content digest mismatch`, `prometheus
skills accept <name>` re-scans and records the edit, `prometheus skills
reset <name>` restores the locked content. `prometheus skills install
--locked` reproduces exactly the locked set and fails on any digest
mismatch.

- `prometheus skills check` reports skills whose upstream changed,
  `orphaned` installs and `invalid_install` paths (the last two without a
  network call).
- `prometheus skills update [<name>]` reinstalls changed skills through the
  whole pipeline; a locally modified skill is skipped (`locally modified;
  use --force`).
- `prometheus skills audit` re-scans every hub skill and quarantines
  `dangerous` ones; `prometheus skills uninstall <name>` removes a hub
  skill; `prometheus skills untrust <root>` removes a trusted project root.
- The same verbs work in a chat: `/skills search terraform`, `/skills
  install <id>`, `/skills tap add my-org/agent-skills`...

### Signatures

A skill may carry `SKILL.sig`, a detached signature over the digest of its
other files, made with a minisign key or a Sigstore keyless identity:
`prometheus skills sign <dir> --key <minisign secret key>` or `--keyless`
(an OpenID sign-in, or GitHub Actions' ambient token in CI).
`skills.signature_policy` decides installs: `warn` (default) installs
unsigned skills with a warning, `require` refuses unsigned ones (exit 77),
and an invalid signature is always refused (`invalid signature`). A valid
signature only proves that someone signed, so it counts as verified only
when the signer is listed in `skills.trusted_signers` (minisign key ids or
public keys, `sigstore:<subject> via <issuer>` identities); any other
signer is `untrusted`: refused under `require`, installed with a warning
under `warn`, and recorded in `skills.lock` either way.
`prometheus skills show <name>` prints the source, ref, digest, signer and
trust.

### Taps, snapshots and publishing

A tap is a GitHub repository of skills (`skills/` by default):
`prometheus skills tap add my-org/agent-skills [--path P]` makes its skills
searchable and installable as `my-org/agent-skills/<skill>` (trust
`community` unless the repository is trusted); `tap list` and `tap remove`
manage them (`skills/taps.toml`).

`prometheus skills snapshot export setup.json` writes every hub skill and
tap; `prometheus skills snapshot import setup.json` on another profile adds
the taps and installs each skill through the pipeline (exit 5 when an entry
failed, each failure reported).

`prometheus skills publish <dir> --to github --repo owner/repo` lints and
scans the skill (refusing lint findings or a `dangerous` verdict), signs it
with `skills.signing_key` when set, and opens a pull request from a fork
branch (never a push to the default branch); `--to clawhub` submits it to
ClawHub (`CLAWHUB_TOKEN`). The pull request or submission URL is printed.

### Choosing skills per surface

`prometheus skills config` lists every visible skill with where it is off
and toggles one per answer: `excalidraw` toggles it everywhere
(`skills.disabled`), `excalidraw telegram` only on Telegram
(`skills.platform_disabled.telegram`). Changes apply from the next session;
`prometheus-agent` cannot be disabled. Without a terminal it exits 2 and
points to `prometheus skills enable|disable <name> [--platform P]`.

### Credential files

A skill may declare files it needs on the execution backend:

```yaml
metadata:
  prometheus:
    credential_files:
      - path: creds/gcp.json        # relative to the profile home
        description: Service account key.
```

They reach a container backend (mounted read-only under the container's
`/prometheus/`) or a remote backend (uploaded under `~/.prometheus/` before
each command, removed when the session ends) only while the skill is
loaded and only with the grant `prometheus grants add skill:gcloud-ops
credential_files`. Without it the session says so once; a missing file is
reported once (`credential file missing: <path>`). The host session's file
tools never read them.

### The embedded catalogs

The binary ships the bundled skills (always available, read-only; eject to
edit) and the official optional catalog: `prometheus skills browse --source
official` lists it offline and `prometheus skills install
official/<category>/<name>` installs one. Skills ported from Hermes are
renamed `hermes-*` → `prometheus-*`; the old names still resolve (a
migrated job naming `hermes-agent` gets `prometheus-agent`).

## The curator

The curator keeps the agent's own skills tidy. It manages only skills the
agent created ([`skill_manage`](../reference/tools.md#skill_manage)) or that you adopted with `prometheus curator
adopt <name>`; it never touches pinned skills, skills a scheduled job names
(paused jobs included), or hub, plugin, external-directory and bundled
skills. A pass marks a skill `stale` after `curator.stale_after_secs`
without use, makes a stale skill active again once it is used, and moves a
skill unused for `curator.archive_after_secs` to `skills/.archive/` (it
never deletes). Every real pass first writes a backup of the skills tree to
`skills/.backups/` (the newest `curator.backup_keep` are kept) and every
run writes `logs/curator/<UTC timestamp>/run.json` and `REPORT.md`.

Automatic passes run from the daemon's scheduler (or `prometheus cron
tick`) when `curator.enabled`, not paused, the last pass is older than
`curator.interval_secs` and the profile has been idle for
`curator.min_idle_secs`; the first observation only records the time.
`curator.consolidate` (or `run --consolidate`) also asks the
`auxiliary.curator` model to merge overlapping skills; the report lists
the rename map `old → new`.

```text
$ prometheus curator status
$ prometheus curator run --dry-run      # the planned transitions; nothing changes
$ prometheus curator run --sync         # a pass now (without --sync it runs in the background)
$ prometheus curator pin deploy-runbook # pause | resume | unpin | adopt | archive | restore
$ prometheus curator prune --days 60    # archive managed skills idle that long
$ prometheus curator purge --days 180   # delete archived skills that old; never automatic
$ prometheus curator ledger --skill deploy
$ prometheus curator rollback L7        # undo one entry; without an id, restore the newest backup
```

Every skill change (the curator's, the agent's [`skill_manage`](../reference/tools.md#skill_manage) writes and
yours from the CLI) is recorded in the skill ledger with the content of
each touched file before and after (`skills.ledger_max_bytes` caps it,
oldest entries first). `rollback L<n>` restores exactly the files that entry
touched and records a `rollback` entry first; if that record fails nothing
changes. `/curator` offers the same commands in any chat; `/curator run`
posts its summary to the chat when the pass ends.

## Using skills in a chat

- `/<skill> [instruction]` loads a skill into your message; `/skill <name>`
  does the same for a skill whose name a built-in command holds.
- Up to five leading skill commands stack into one message:
  `/github-pr-workflow /test-driven-development fix issue 123`. Parsing stops
  at the first word that is not a skill, so `/pdf /tmp/scan.pdf extract
  tables` loads only `pdf`. A sixth skill stays part of the text, with a
  notice to use a bundle.
- `prometheus chat -s a,b` and the profile key `skills.auto_load` load skills
  into the session before its first turn. They are rendered once into the
  session's frozen prompt, so they never break the prompt cache. A name that
  does not resolve is skipped with a warning.

### Bundles

A bundle loads several skills with one command. It is a file
`<profile>/skill-bundles/<slug>.toml`:

```toml
description = "Backend work."
skills = ["github-code-review", "test-driven-development", "github-pr-workflow"]
instruction = "Run the tests before you finish."
```

`/backend-dev refactor auth` then loads all three skills, the instruction
and your text into one message (no stacking limit). A listed skill that is
not installed is skipped and named (`skipped: <name>`). A bundle's command
wins over a skill of the same name, never over a built-in command.

```console
$ prometheus skills bundles create backend-dev --skill prometheus-agent --instruction "Run the tests."
$ prometheus skills bundles list
/backend-dev  prometheus-agent
$ prometheus skills bundles delete backend-dev
```

`/bundles` lists them in a chat; `prometheus skills bundles reload` reports
added, changed and removed bundles.

### Reloading

Skills copied into a root are usable at once. `/reload-skills` (or
`prometheus skills reload`) rescans every root and bundle and reports what
was added, removed and changed. The running session's system prompt does
not change: the model learns of new skills through a note on your next
message, and the next session's index includes them.

## Skills the agent writes

The agent writes skills with
[`skill_manage`](../reference/tools.md#skill_manage), only below the profile's
`skills/` directory, and every write is validated and security-scanned
(a failed scan refuses the write).

- `/learn <sources>` asks the agent to distil a skill from directories,
  files, URLs or the conversation. Large sources (over 50,000 characters)
  become a knowledge base: a short `SKILL.md` index plus one file per topic
  under `references/`. Learning the same topic again patches the skill.
- After a long tool-using conversation the agent reviews it in the
  background and may save or improve skills (`skills.creation_nudge_interval`).
  `/refine [focus]` starts that review, and the memory review, now; their
  summary (skills created or patched, memories saved or proposed) is
  posted to the chat and the running session is not changed. A second
  `/refine` while one runs answers `a review is already running`.
- With `skills.write_approval = true` (`/skills approval on`), every write is
  staged in `<profile>/pending/skills/` instead of applied.
  `/skills pending` lists staged writes, `/skills diff <id>` shows one,
  `/skills approve <id>|all` applies them and `/skills reject <id>|all`
  drops them. `prometheus skills pending list|diff|approve|reject` does the
  same from a terminal.

## Authoring a skill

A skill directory is named after the skill:

```text
skills/devops/deploy/
├── SKILL.md
├── references/   # longer documentation the skill points to
├── templates/    # files to copy or fill in
├── scripts/      # helper scripts (run sandboxed)
└── assets/
```

`SKILL.md` follows the agentskills.io format:

```markdown
---
name: deploy
description: Build, tag and ship the service.
category: devops
related_skills: [systematic-debugging]
---

## When to Use

When the user asks to release or deploy the service.

## Procedure

1. Run the test suite.
2. Tag the release.
3. Ship it.

## Pitfalls

Never deploy with failing tests.

## Verification

The health endpoint reports the new version.
```

Rules, checked by `prometheus skills lint [<path>...]` (`--strict` exits 1
on any finding, and CI runs it on every bundled skill):

- the frontmatter parses and has `name` and `description`;
- `name` equals the directory name (`^[a-z0-9][a-z0-9._-]*$`);
- the description fits `skills.index_description_chars` (default 60) and ends
  with a period;
- no marketing words: `powerful`, `comprehensive`, `seamless`,
  `revolutionary`, `cutting-edge`, `state-of-the-art`, `advanced`;
- every `related_skills` entry resolves;
- no machine-local home paths (use `${PROMETHEUS_SKILL_DIR}` for files of the
  skill);
- `SKILL.md` stays under 100,000 characters.

```console
$ prometheus skills lint --strict
<n> checked, no findings
```

`prometheus skills doctor` reports skills that do not load at all, with the
file and line.

A skill can carry a `blueprint` block (a scheduled job with typed slots,
see [Scheduled jobs](cron.md#blueprints)). It never schedules anything by
itself: the blueprint is offered in `/suggestions` and `prometheus cron
suggestions` (and as `skill:<name>` in `/blueprint`), and a job exists only
once you accept it.
