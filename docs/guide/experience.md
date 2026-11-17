# The experience library

Harness evolution learns something from every task it improves. The
experience library carries that to later work of the same **domain**:
after a job's selection, a reflector session reads the whole search and
distils at most one transferable lesson, and when you adopt the proposal
the adopted harness and its `EXPERIENCE.md` become an entry of
`<profile>/experience/<domain>/`. Later evolution jobs of the domain show
the library to their proposers, and (opt-in) later sessions of the
domain start with its best lessons. The library exists only where
`evolve.enabled = true`, and it is profile-local.

## Domains

A domain is a lower-case name such as `accounting` or `rust-refactors`.
Anything you type is normalised: `--domain "Business / Financial Ops"`
becomes `business-financial-ops`, and an empty name is `general`. A
session gets a domain from `prometheus chat --domain <name>`, the
`/domain <name>` slash command, or the `domain` of the scheduled job
(`prometheus cron add ... --domain <name>`) or kanban task
(`prometheus kanban create ... --domain <name>`) that started it. An
evolution job takes `--domain`, else its trigger's domain, else its
session's tag, else one auxiliary call that files it under one of the
profile's existing domains or `general` (never a new one). Jobs of one
domain run one at a time in queue order, so each job sees exactly the
entries adopted before it started.

## Entries

An entry is written only when a proposal that carries a lesson is
adopted, by you or by policy; a rejected, superseded or unchanged
proposal writes nothing, and `prometheus evolve review <proposal>` says
`no transferable lesson; nothing will be added to the experience
library` when there is none. Each entry holds:

- `EXPERIENCE.md`: the task, the chosen candidate, its components, why
  it was chosen, what each edit did (`helped`, `hurt`, `no observed
  effect` or `confounded`) and the lesson's cue, lesson, evidence and
  confidence;
- `entry.toml`: its id (`<yyyymmdd>-<task-slug>-<6 hex>`), domain, job,
  harness version, mode, k, confidence, status, pinned and times shown;
- `harness/`: the whole adopted harness tree.

Nothing derived from a grader may enter an entry: its tree, its
`EXPERIENCE.md` and the selector's reason pass the leak globs, the
verdict tripwire and the secret scanner first, and a hit records
`lesson_rejected` on the proposal and writes no entry (the adoption
itself stands). A domain keeps at most `experience.max_entries_per_domain
= 50` entries, evicting the oldest unpinned one. `INDEX.md` lists the
domain's visible entries, and a job's evidence gets a snapshot of the
newest `experience.max_entries_in_evidence = 20` of them (pinned first).

## Managing the library

```console
$ prometheus experience list
no experience entries
$ prometheus experience domains
no experience domains
$ prometheus experience show 20261029-reconcile-q3-ledger-a1b2c3 # exits 1
error: no experience entry '20261029-reconcile-q3-ledger-a1b2c3'
$ prometheus experience rebuild # exits 78
error: harness evolution is disabled for profile 'default'; set evolve.enabled = true
```

`prometheus experience list [--domain D] [--all]` shows each entry's id,
domain, status, confidence, pin and times shown with its cue (`--all`
adds rolled-back and disabled entries); `show <entry>` prints one;
`domains` counts entries per domain; `pin` and `unpin` keep an entry from
eviction and rollback; `forget <entry>` removes it (its harness version
stays); `disable <entry>` hides it from jobs and new sessions (status
`disabled`); `restore <entry>` makes a rolled-back or disabled entry
active again. The dashboard and desktop use the same operations over the
client protocol (`experience.list`, `experience.show`,
`experience.domains`, `experience.pin`, `experience.forget`,
`experience.disable`, `experience.restore`).

`prometheus harness rollback` demotes every unpinned entry whose harness
the active version no longer descends from to `rolled-back`, hiding it
from jobs and sessions until you restore it.

`prometheus experience export <domain> <FILE.tar.zst>` writes a domain to
an archive, and `prometheus experience import <FILE> [--domain D]` adds
one to another profile: every entry's harness must validate and pass the
leak, verdict and secret checks, or the whole archive is refused (exit
65) and nothing is written. Imported entries get new ids and record
`imported_from`.

`prometheus experience rebuild [--domain D]` re-creates the missing
entries of adopted proposals from the proposal records and their harness
versions, without any model call (a resumed job does the same for its
domain). A forgotten or evicted entry is not brought back.

## Lessons in sessions

With `experience.session_lessons = true`, a new session with a known
domain starts with a `## Lessons from earlier work in <domain>` prompt
layer: the `cue → lesson` lines of at most
`experience.session_lessons_max = 5` visible entries of high or medium
confidence (pinned first, then newest), capped at 2,000 characters. The
layer is decided once, at the session's start, and kept for its life: a
lesson adopted while a session is open reaches only the next session.
Each injection counts in the entries' `times_shown` and is recorded as an
`experience.session_lessons.injected` trajectory event; the library's own
changes go to `<profile>/experience/events.jsonl`.
