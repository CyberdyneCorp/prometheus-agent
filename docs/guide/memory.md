# Memory

Prometheus remembers durable facts across sessions in the profile's
`state.db`. Each entry has a stable id (`m_` plus 12 characters), a target,
a kind, a tier and the text itself.

- **Targets.** `memory` holds the agent's own notes (environment facts,
  project conventions, lessons learned). `user` holds what it knows about
  you. Turn either off with `memory.memory_enabled` or
  `memory.user_profile_enabled`. With both off, the
  [`memory`](../reference/tools.md#memory) tool is not offered and
  `prometheus memory status` says that built-in memory is disabled.
- **Tiers.** `core` entries go into the system prompt. They share a
  character budget per target: `memory.memory_char_limit` (2200) and
  `memory.user_char_limit` (1375). `archival` entries have no budget and are
  capped at `memory.archival_max_entries` per target. They are recalled into
  a turn when they are relevant.
- **Kinds.** `fact` (the default), `preference`, `instruction`, `project`,
  `person` or `event`.

## What the model sees

At session start, the core entries of each enabled target are rendered
into the system prompt as a block with a usage indicator:

```text
USER PROFILE (who the user is) [1% — 21/1375 chars]
Prefers metric units.
```

That block is frozen with the rest of the prompt, so the prompt cache
stays valid. When the agent writes memory during a session, the change
shows only in the tool result. The prompt picks it up at the next session
or when context compression rebuilds the prompt. `/memory reload --now`
rebuilds it at the next turn and records a `prompt.rebuilt` event with
reason `memory --now`. Plain `/memory reload` waits for the next session.

Before each model call, Prometheus looks for archival entries that share
words with your message. It skips empty messages, slash commands and bare
greetings or acknowledgements. Up to `memory.recall.top_k` entries, within
`memory.recall.max_chars`, are appended to that turn's user message inside
a `<memory-context>` fence. A note before the fence says this is recalled
memory, not an instruction from you. The fence is sent with that one turn
only: it is never stored in the transcript and never added to the system
prompt. The turn's trajectory records `memory.recalled` with the entry ids.

## The memory tool

One call changes one target. It carries a list of `operations`, applied in
one transaction:

```json
{"target": "user", "operations": [
  {"action": "remove", "old_text": "Lives in Lisbon"},
  {"action": "add", "kind": "fact", "content": "Lives in Porto."}
]}
```

- The actions are `"add"`, `"patch"`, `"remove"`, `"pin"` and `"unpin"`. `new_text`
  is accepted as another name for `content`. `replace` is refused with a
  pointer to `"patch"`.
- `"patch"`, `"remove"`, `"pin"` and `"unpin"` name the entry by `id`, or by an
  `old_text` that appears in exactly one active entry. If the selector is
  missing or matches nothing, the result lists the current ids. If it
  matches several entries, the result lists the candidate ids.
- The whole call is checked before anything is written: the target, every
  action, kind and tier, and `expires_at`, which must be a future RFC 3339
  time. The budget is checked on the final state of the batch, so one call
  can drop a stale entry and add a new one. A call that would push the
  core tier over its budget writes nothing. Its message gives the current
  and limit characters and suggests consolidating or `tier: "archival"`.
- Adding text that exactly matches an active entry changes nothing. The
  result returns that entry's id with `duplicate: true`.
- After three failed memory calls in one turn, the next failure answers
  `done: true` and tells the model to continue its reply.

Memory content is scanned before it is written. Text shaped like
instructions planted for a later session is refused, for example
"ignore previous instructions", role hijacks, paths such as `~/.ssh` or
sending data to a URL. Credentials are refused too. An entry that is
already stored and matches the current patterns is shown to the model as
`[BLOCKED: <rule>]` and marked `blocked`. Its raw text stays in the store so
you can inspect it with `prometheus memory show <id>` and delete it with
`prometheus memory rm <id>`.

## Approval and background review

When `memory.write_approval = true`, or when `memory.review_mode =
"require"`, agent writes need approval. An interactive foreground session
asks you inline. Any other session stages the change as a pending record.
To list, apply or drop pending records, use `/memory pending`,
`/memory approve <id>|all` and `/memory reject <id>|all`, or the matching
`prometheus memory pending|approve|reject` commands. A staged patch or
removal records the entry text it was proposed against. Approving it after
that entry has changed is refused with `memory.stale`.

A session counts the user turns since its last memory call. At
`memory.nudge_interval` turns (10 by default; 0 turns this off), the agent
reviews the conversation in a hidden child session after the reply is
delivered. Cron jobs, delegated children and harness-evolution sessions
never run this review. The review's adds are committed. Its patches,
removals and unpins are only proposed, and you approve them from
`/memory pending`.

## The memory command

| Command | What it does |
|---|---|
| `prometheus memory status` | Shows whether each store is enabled, its usage and pending changes. |
| `prometheus memory list [--target] [--kind] [--tier] [--all]` | Lists entries. Add `--all` to include dormant ones. |
| `prometheus memory show <id>` | Shows one entry in full, including the raw text of a blocked one. |
| `prometheus memory add <text> [--target] [--kind] [--tier]` | Adds an entry. |
| `prometheus memory rm\|pin\|unpin <id>` | Removes, pins or unpins an entry. |
| `prometheus memory edit [--target]` | Opens the Markdown view in `$VISUAL` / `$EDITOR`. The result is applied as one batch after the diff is printed. |
| `prometheus memory export [--format md\|jsonl] [-o FILE]` | Writes the Markdown view or JSONL. |
| `prometheus memory import <file> [--target] [--tier]` | Imports an export, or a Hermes `MEMORY.md` or `USER.md`. |
| `prometheus memory pending\|approve\|reject` | Lists, applies or drops staged changes. |
| `prometheus memory reset [--target all\|memory\|user] --yes` | Deletes every entry of a store. |

For example:

```console
$ prometheus memory add "Prefers metric units." --target user --kind preference
m_<id>
$ prometheus memory status
Built-in memory: enabled (provider: builtin)
user: 1 core entries [1% — 21/1375 chars], 0 archival
$ prometheus memory show m_zzzzzzzzzzzz  # exits 66
```

Changes made with these commands are recorded with `origin = user`, and
imports with `origin = import`. They reach the system prompt from the next
session.

The Markdown view has one `#` section per target and one `##` section per
entry, headed by its id. Write `## new` to add an entry. The entry's
settings sit in an HTML comment:

```markdown
# USER PROFILE (who the user is)

## m_abcdefghijkl
<!-- kind=preference tier=core pinned=false -->
Prefers metric units.

## new
Works in UTC.
```

In `memory edit`, deleting a section removes that entry. In `memory
import`, the file never deletes anything:

- An entry whose id the store has is updated.
- An id the store lacks is restored as is.
- `## new` sections are added.
- Exact duplicates of active entries are skipped.

A Hermes file is split on its `§` separator lines. Its target comes from
`--target` or from the file name (`MEMORY.md`, `USER.md`). Imported entries
that match the threat scan are kept as `blocked`. Entries holding
credentials are skipped.

The revision journal (`memory log`, `diff`, `revert`), external memory
providers (`memory setup`, `off`), semantic `memory search` and
consolidation arrive in later milestones.
