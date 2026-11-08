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

Before each model call, Prometheus looks for archival entries relevant to
your message. It skips empty messages, slash commands and bare greetings
or acknowledgements. By default entries are ranked by full-text relevance
(FTS5 BM25), so an entry must share a word with your message. Set
`memory.recall.embedding_model` (an alias, `provider/model`, or a model of
the session's provider that serves `POST /embeddings`) for hybrid ranking:
`0.6 * cosine similarity + 0.4 * normalized BM25`, which also recalls a
paraphrase such as "what's my kid called?" for "The user's daughter is
named Ana.". Entries scoring below `memory.recall.min_score` (0.35) are
dropped. Embeddings are stored in `state.db` per entry and model and
recomputed when the content or the model changes. Text sent to a remote
embedding model is redacted first. If the embedding call fails, recall
falls back to full-text ranking with a warning. Up to `memory.recall.top_k` entries, within
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

## Reviewing what changed

Every committed change to an entry is recorded in a revision journal in
the same transaction: a revision number that only grows, the entry, the
operation (add, patch, remove, or update for pin, tier and other
fields), the entry before and after, the origin and the session.

With `memory.review_mode = "summary"` (the default), a session in which
the agent changed memory prints one line when it ends or is resumed: the
line interface and `chat -z` print it as the session ends, the TUI as it
closes a session and once more for each session it held as it quits, and
both when `/resume` (or `-r`/`-c`) opens such a session. A messaging chat
gets it as a message when `/new` or `/reset` ends the session and when
`/resume` brings it back.

```text
Memory: +2 ~1 -0 (review with /memory review)
```

`/memory review` lists that session's changes as diffs. Keep one with
`/memory review keep <rev>`, or undo it with `/memory review revert <rev>`
(`all` works for both). Reviewed changes are not summarized again.
`review_mode = "off"` shows no summary, and `"require"` stages every agent
change for approval instead of committing it.

`prometheus memory revert <rev>` applies the inverse of any revision as a
new revision with `origin = user`: a patch gets its old text back, an add
is removed and a removal is restored.

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
| `prometheus memory log [--since <rev\|duration>] [--origin]` | Lists revisions, oldest first. |
| `prometheus memory diff [--since <rev\|duration>\|--session <id>]` | Shows the changes as a unified diff per entry. |
| `prometheus memory revert <rev>` | Undoes a revision as a new `origin = user` revision. |
| `prometheus memory search <query> [--explain]` | Ranks archival entries as recall would, with their scores. |

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

## Background review controls

`[memory.review]` tunes the automatic review that runs after
`memory.nudge_interval` turns without a memory call: `enabled = false` skips
it (`/refine` still works); `max_input_tokens` bounds the input one review
replays over all its model calls (unset: 75% of the review model's window,
at most 600000, or 120000 when the window is unknown; 0 or negative is
unlimited), and the review stops before the call that would pass it;
`extra_tools` lists tools the review may use when the session itself has
them; `defer = "auto"` holds a review whose model runs on a local
inference server until the machine is idle, at most `defer_max_age_secs`
(1800). A second review of a session while one waits or runs is dropped.
The review runs on `[auxiliary.memory]` when it names a provider or model
(with a digest of the recent conversation), else on the session's own model
with the whole transcript; its calls are recorded with purpose
`background_review`.

## External memory providers

Next to the built-in store a profile can run one external provider: a
`memory-provider` WASM plugin, or a Hermes `MemoryProvider` hosted by the
Python bridge. `memory.provider` names it (`builtin`, the default, means
none); a name that is not an installed plugin fails validation (exit 78)
and suggests `prometheus plugins install <name>`.

- `prometheus memory setup [provider]` asks for the provider's settings:
  secret fields go only to the profile's secrets store (as
  `<PROVIDER>_<KEY>`), every other field only to
  `memory.providers.<id>.<key>` in `config.toml`; then `memory.provider` is
  set. `prometheus memory off` goes back to built-in memory. Both apply from
  the next session.
- Each turn the provider's recall (bounded by
  `memory.provider_prefetch_timeout_ms`, 8000 by default: a slow provider is
  cut off and the turn goes on) joins the built-in recall inside the
  `<memory-context>` fence, up to `memory.provider_recall_max_chars`
  (100000), cut beyond it, or with `memory.provider_recall_spill = true`
  replaced by a preview and the path of a file under `cache/` holding all
  of it.
- Completed turns, committed (never staged) memory changes and delegated
  tasks reach the provider on a background queue, never inline; closing a
  session drains it for at most 5 seconds. Everything sent is redacted
  first and skill-invocation text is stripped.
- The provider's system-prompt block follows the built-in memory blocks
  and freezes with the prompt. Its tools join the `memory` toolset; one
  named like a core or another registered tool is refused, and
  `prometheus memory status` reports the collision.
- Before a compaction the provider gets the messages about to be
  summarized. With `memory.require_checkpoint = true`, a compaction no
  provider confirms is not committed and a warning says so.
- A provider instance serves only the profile it was started for.
- A disabled, unavailable or failing provider leaves the session on
  built-in memory with one warning (per profile, provider and reason).

## Context engines

`context.engine` can name a loaded `context-engine` plugin (by its id)
instead of `builtin`. The engine compacts through the host, which validates
its output before committing it, and may choose which stored messages each
request sends: the choice shapes only the outgoing request, never the
stored transcript. Its tools join the `context_engine` toolset. An engine
that fails to start leaves the session on the built-in engine.

Consolidation arrives in a later milestone.
