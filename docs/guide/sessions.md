# Sessions

Every conversation with Prometheus is a **session**, stored in the
profile's `state.db` (SQLite) with large payloads under `blobs/`. This guide
covers listing, searching, exporting and cleaning up sessions, and what the
store does on its own to keep your history safe.

Every command below is run by the documentation tests against a fresh home,
in order. Lines starting with `$` are commands; the lines after a command
are (part of) its output, with `<...>` standing for text that varies.
Examples in `text` blocks show output from a home that already has
sessions.

## Session ids and references

A session id looks like `ses_01JC3W8Q5ZK7Y2V4N6B8D0F1H3` and never changes,
whichever surface (CLI, TUI, gateway, cron...) created the session. Every
command that takes a session accepts:

1. the full id,
2. a title (`Refactor`; a bare title picks the newest of `Refactor`,
   `Refactor #2`, `Refactor #3`...),
3. an id prefix of at least 8 characters (`ses_` optional).

An unknown reference exits 66, an ambiguous prefix exits 2:

```console
$ prometheus sessions show nope # exits 66
error: no session matches 'nope'
hint: list sessions with `prometheus sessions list`
```

## Listing and inspecting

`sessions list` shows the most recently active sessions first. Archived,
hidden and integration sessions (one-shot runs and tool callers, tagged
`oneshot` or `tool`) only appear with `--all`:

```console
$ prometheus sessions list
no sessions
$ prometheus sessions list --all --limit 5 --json
{"schema":"prometheus.sessions.v1","sessions":[]}
```

With sessions, a listing looks like this (id, last activity, source,
message count, title):

```text
ses_01JC3W8Q5ZK7Y2V4N6B8D0F1H3  2026-10-30 09:12  cli             2 msgs  Refactor
ses_01JC3W7M2XE4R6T8Y0U2I4O6P8  2026-10-29 17:40  cli (tool)      4 msgs  -
```

`sessions show <ref>` prints one session and its lineage (the chain of
compression continuations); `sessions tree <ref>` prints every fork,
continuation and delegated child; `sessions stats <ref>` prints token and
cost totals for the session and for its whole lineage, counting
continuations, forks and delegated children under their root. All of them
accept `--json`.

`rename`, `pin`, `unpin`, `archive` and `unarchive` change a session.
Titles are unique per profile and at most 100 characters; a pinned session
is never pruned.

### Browsing

`prometheus sessions browse [--source <s>] [--all]` lists sessions newest
first with their title, relative last activity, message count, source
and a status tag: `live` (not ended), `done`, `intr` (ended
`interrupted`), `err` (ended with an error) or `empty` (no messages).
Type text to narrow the list to matching titles and full-text hits,
type a row number to resume that session in the chat, or press Enter to
leave. It needs a terminal (exit 2 otherwise; use `sessions list`).

## Searching

`sessions search <query>` searches every message of the profile:

```console
$ prometheus sessions search "chat-send" --limit 5
no matches
$ prometheus sessions search "NOT (broken" --json
{"schema":"prometheus.search.v1","hits":[],"mode":"word"}
```

Queries take words, `"quoted phrases"`, a trailing `*` for prefixes and
`AND`, `OR`, `NOT` between terms. Anything else is treated as text, so no
query can fail: `chat-send` searches the phrase `"chat-send"`, an unbalanced
quote is ignored and a dangling `NOT` is dropped. Queries in Chinese,
Japanese or Korean, and terms shorter than 3 characters, use a trigram index
that matches inside words.

Hits are listed best match first (`--sort newest|oldest` to change it), 20
at a time (`--limit`, `--offset`), with a snippet around the match:

```text
ses_01JC3W8Q5ZK7Y2V4N6B8D0F1H3  msg_01JC3W8R0A1B2C3D4E5F6G7H8J  user  2026-10-30 09:10
  Split the «parser» into modules
```

Filters: `--source` and `--exclude-source`, `--role`, `--after` and
`--before` (RFC 3339 or `YYYY-MM-DD`), `--session <ref>`, `--pinned` and
`--no-archived`. Rewound messages are left out unless `--include-inactive`;
messages summarised by compaction stay searchable.

The search indexes are derived from the stored messages. If one is damaged
Prometheus keeps saving messages, marks the indexes stale and answers
searches by scanning the messages (slower, never wrong) until they are
rebuilt:

```console
$ prometheus sessions repair --indexes
rebuilt the search indexes (0 messages)
```

`prometheus doctor --fix` runs the same rebuild when it finds stale indexes.

## Exporting and saving

`sessions export [<ref>...|--all]` writes sessions to stdout, or to a new
file with `-o FILE` (an existing file is never overwritten; exit 73):

| `--format` | Content |
|---|---|
| `jsonl` (default) | Portable: a `{"format":"prometheus.sessions","version":1}` header, then each session, its messages, usage rows and lineage, one JSON object per line |
| `md` | A readable Markdown transcript, tool calls and results collapsed |
| `html` | One self-contained page (no external resources) |
| `prompts` | Only your messages, one JSON line each |
| `trace` | The trajectory event grammar (`session.start`, `user.message`, ...) |

Only the active transcript is exported unless `--include-inactive`.
`--redact` replaces the profile's secret values and anything shaped like a
credential (API keys, tokens, private keys) with `[REDACTED]`:

```console
$ prometheus sessions export --all --format md --redact
```

Inside a session, `/save [json|md|html] [file] [redact]` writes the current
session to `file`, or to `<cwd>/<title>-<YYYYMMDD-HHMM>.<ext>`, redacted
when `redact` is given. A bare `/save` prints its usage.

## Rewinding

Rewinding is a soft delete: the messages disappear from what the model sees
but stay in the database, so a rewind can be undone.

```text
$ prometheus sessions rewind Refactor                      # back to before the last user message
$ prometheus sessions rewind Refactor --to msg_01JC...     # back to before this user message
$ prometheus sessions rewind Refactor --restore msg_01JC... # undo that rewind
```

`/undo` in a session does the same as the first form. A rewind is refused
(exit 75) while another process is running a turn in the session, and a
restore is refused once new messages were added after the rewind.

## Deleting, pruning and archiving

`sessions delete <ref>` asks you to type `delete` (or pass `--yes`; without
a terminal it exits 4). It removes the session's messages, usage rows and
blobs, and its delegated children. Forks keep living on their own (marked
`parent_deleted`); compression continuations are deleted only with
`--with-lineage`. A session in use by a running turn or compaction is not
deleted (exit 75).

`sessions prune --older-than <days>` deletes ended, unpinned sessions
inactive for longer than that; a chain of compression continuations goes
only as a whole, once every part of it qualifies:

```console
$ prometheus sessions prune --older-than 90 --dry-run
would delete 0 session(s)
```

Two settings in `config.toml` make this automatic. The daily maintenance
pass prunes by `sessions.retention_secs` (default `0`: never) and archives
sessions idle for `sessions.auto_archive_secs` (default 7 days; `0`
disables). Resuming an automatically archived session unarchives it; an
archive you made by hand stays.

```console
$ prometheus config set sessions.retention_secs 7776000
$ prometheus config get sessions.retention_secs
7776000
```

Resuming is refused for lineages with more than
`sessions.max_resume_messages` active messages (default 20000; compact with
`/compress` or fork instead), and exports held in memory (dashboard, RPC)
stop at `sessions.max_export_messages`; `sessions export -o FILE` streams
any size.

## Safety nets

- **Integrity check.** The first time a process opens `state.db` it runs
  SQLite's `quick_check`. A damaged file is never repaired in place: it is
  moved to `<profile>/quarantine/state-<time>.db`, every readable session
  and message is copied into a fresh `state.db`, and `prometheus doctor`
  reports the quarantine path with the rows salvaged and lost.
- **Replaced files.** If `state.db` is replaced while Prometheus has it
  open (for example a backup copied over it), the next write fails with
  `store.replaced` (exit 75) instead of writing into the restored file.
  Restart to use the new file.
- **Busy store.** Writes wait for another process holding the database: up
  to 20 s for session changes, 60 s for messages and 0.5 s for activity
  updates, then fail with `store.busy` (exit 75).

`sessions vacuum` compacts the database file after large deletions:

```console
$ prometheus sessions vacuum
vacuumed <path>
```
