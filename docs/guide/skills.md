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
