# Session Storage

## Purpose
Session storage is the per-profile durable store (`<profile>/state.db`, SQLite in WAL mode, plus a content-addressed `blobs/` directory) that holds session metadata, the canonical transcript, lineage (compression continuations, forks, delegates, resets, imports), titles, usage events and derived search indexes. The key decisions are: canonical rows are authoritative and every index (word, trigram, semantic) is derived, rebuildable and fails open; the store is only ever opened through an explicit `ProfileScope`; one session-identity format for every surface; and lineage is a typed graph so forks, compression chains and usage roll-ups are answered by one query model instead of per-feature heuristics.

## ADDED Requirements

### Requirement: Store opened only through ProfileScope
The system SHALL open the session store only from an explicit `ProfileScope`, at `<profile>/state.db` with blobs under `<profile>/blobs/`, SHALL provide no process-wide default store path, and SHALL refuse to open a database file whose recorded `profile_id` (stored in the `store_meta` table at creation) differs from the opening scope's profile id.
**Milestone:** M1

#### Scenario: Copied database refused under another profile
- **GIVEN** `profiles/work/state.db` was copied over `profiles/home/state.db`
- **WHEN** a `home` session operation opens the store
- **THEN** the system SHALL fail with error code `store.profile_mismatch` naming both profile ids and SHALL NOT write to the file

#### Scenario: Two profiles in one gateway use separate stores
- **GIVEN** one gateway process serving profiles `work` and `home`
- **WHEN** a message is appended to a `home` session
- **THEN** the row SHALL be written to `profiles/home/state.db` and SHALL NOT be visible from any `work` store query

### Requirement: Owner-only file permissions
The system SHALL create `state.db`, `state.db-wal`, `state.db-shm` and every blob file with mode `0600` and the `blobs/` directory with mode `0700` on POSIX hosts, SHALL tighten wider modes on existing files at open by `chmod` of the path (skipping symlinks and non-regular files), and SHALL restrict the files to the owning user's ACL on Windows.
**Milestone:** M1

#### Scenario: Loose permissions tightened
- **GIVEN** an existing `state.db` with mode `0644`
- **WHEN** the store is opened writable
- **THEN** the file mode SHALL be `0600` and a WARN log entry SHALL name the path and the previous mode

### Requirement: Embedded versioned migrations
The system SHALL record the schema version in `store_meta.schema_version`, SHALL apply embedded, ordered, forward-only migrations inside one transaction per step after copying the database to `<profile>/backups/state/state-v<old>-<timestamp>.db` (keeping the newest 3 backups), and SHALL open a store whose version is newer than the binary supports in read-only mode, refusing writes with error code `store.schema_too_new`.
**Milestone:** M1

#### Scenario: Older store upgraded with backup
- **GIVEN** a `state.db` at schema version 3 and a binary supporting version 5
- **WHEN** the store is opened writable
- **THEN** the system SHALL write a backup file, apply migrations 4 and 5, and record `schema_version = 5`

#### Scenario: Downgraded binary cannot corrupt a newer store
- **GIVEN** a `state.db` at schema version 7 and a binary supporting version 5
- **WHEN** the user runs `prometheus sessions list`
- **THEN** the system SHALL list sessions read-only and any write attempt SHALL fail with `store.schema_too_new` naming both versions

### Requirement: Single session identity format
The system SHALL identify every session, regardless of surface or origin, as `ses_` followed by a 26-character Crockford base32 ULID, and every message by a durable `msg_`-prefixed ULID that is preserved by compaction, forks, export and import; the surface, job or chat that created the session SHALL be recorded in typed columns (`source`, `origin`), never encoded in the id.
**Milestone:** M1

#### Scenario: Cron session id
- **WHEN** scheduled job `daily-digest` starts a session
- **THEN** the session id SHALL match `^ses_[0-9A-HJKMNP-TV-Z]{26}$`, `source` SHALL be `cron` and `origin.job_id` SHALL be `daily-digest`

### Requirement: Session record and source enum
The system SHALL store for each session at least `id`, `profile_id`, `source` (one of `cli`, `tui`, `gateway`, `cron`, `subagent`, `acp`, `api`, `desktop`, `dashboard`, `eval`, `harness`, `import`), `origin` (typed JSON: platform/chat/thread/user for gateway, job id for cron, parent for subagent), `model`, `provider`, `cwd`, `git_repo_root`, `git_branch`, `title`, `title_source`, `started_at`, `last_activity_at`, `ended_at`, `end_reason` (one of `user_exit`, `new_session`, `compression`, `budget_exhausted`, `error`, `interrupted`, `idle_timeout`, `job_complete`), `pinned`, `archived` (`none`, `user`, `auto`), `hidden`, `bot_chat` (boolean, default false; the canonical Bot Chat marker owned by `bot-mode`), and `system_prompt_hash`, and SHALL reject an unknown `source` or `end_reason` value.
**Milestone:** M1

#### Scenario: Unknown source rejected
- **WHEN** a caller creates a session with `source = "telegrm"`
- **THEN** the system SHALL fail with error code `store.invalid_enum` naming the field and the allowed values

### Requirement: First end reason wins
The system SHALL set `ended_at` and `end_reason` only when `ended_at` is null, and reopening a session (resume) SHALL clear `ended_at`, `end_reason` and an `auto` archive flag while preserving a `user` archive flag.
**Milestone:** M1

#### Scenario: Second end call ignored
- **GIVEN** a session ended with `end_reason = "compression"`
- **WHEN** another writer ends it with `end_reason = "user_exit"`
- **THEN** the stored `end_reason` SHALL remain `compression`

### Requirement: Transactional message persistence
The system SHALL append a message and update the session's counters (`message_count`, `tool_call_count`, `last_activity_at`) in one write transaction, SHALL insert a batch of messages all-or-none, SHALL assign each message a strictly increasing per-session `seq`, and SHALL store tool calls, reasoning items and provider-specific opaque payloads as typed JSON columns with blobs larger than 64 KiB moved to the blob store and referenced by SHA-256.
**Milestone:** M1

#### Scenario: Batch failure leaves no partial rows
- **GIVEN** a batch of 5 messages whose 4th violates a constraint
- **WHEN** the batch is appended
- **THEN** no message from the batch SHALL be stored and the session counters SHALL be unchanged

#### Scenario: Large tool output spilled to blob
- **WHEN** a tool result of 200 KiB is appended
- **THEN** the message row SHALL hold a `blob:sha256:<hex>` reference and `<profile>/blobs/<first 2 hex>/<hex>` SHALL contain the bytes

### Requirement: Content-addressed blob store with garbage collection
The system SHALL store media, attachments, oversized payloads and trajectories in `<profile>/blobs/<aa>/<sha256>` written via temp file plus atomic rename, SHALL verify the hash on read and treat a mismatch as error `store.blob_corrupt`, and SHALL delete blobs unreferenced by any canonical row only during maintenance and only when older than 24 hours.
**Milestone:** M1

#### Scenario: Freshly written blob not collected
- **GIVEN** a blob written 10 minutes ago whose referencing message has not yet committed
- **WHEN** maintenance runs
- **THEN** the blob SHALL NOT be deleted

### Requirement: Typed session lineage
The system SHALL record lineage as `parent_session_id`, `lineage_kind` (one of `compression`, `fork`, `delegate`, `reset`, `import`) and `root_session_id` on every child session, SHALL compute `root_session_id` as the parent's root at insert time, and SHALL expose `lineage(session_id)` returning the ordered chain from root to tip and `tree(root_id)` returning every descendant with its kind.
**Milestone:** M1

#### Scenario: Compression chain tip
- **GIVEN** session A continued by compression into B, and B into C
- **WHEN** `lineage(A)` is requested
- **THEN** the result SHALL be `[A, B, C]` and the compression tip SHALL be `C`

#### Scenario: Fork not mistaken for continuation
- **GIVEN** session A ended with `end_reason = "compression"` and a child F with `lineage_kind = "fork"`
- **WHEN** the compression tip of A is requested
- **THEN** F SHALL NOT be returned as a compression continuation

### Requirement: Non-destructive in-place compaction
The system SHALL support compaction under the same session id by marking summarised messages `active = 0, compacted = 1` and inserting the replacement messages as new active rows in one transaction, re-sequencing newer rows after the compacted set, and SHALL abort without changes with error `store.compaction_conflict` when any message the compactor read has since changed (compare-and-swap on message ids and content hashes).
**Milestone:** M1

#### Scenario: Concurrent append during compaction
- **GIVEN** a compactor that read messages 1-40 of a session
- **WHEN** message 41 is appended before the compaction commits
- **THEN** the compaction SHALL commit and message 41 SHALL be sequenced after the inserted summary rows

#### Scenario: Rewritten source row aborts compaction
- **GIVEN** a compactor that read message 12
- **WHEN** message 12 is rewound before the compaction commits
- **THEN** the compaction SHALL fail with `store.compaction_conflict` and no rows SHALL change

### Requirement: Turn lease and compaction lock
The system SHALL grant at most one live turn lease and one compaction lock per session, each a row carrying holder identity (process id, process start time, host id) and an expiry (turn lease default 120 s renewed every 30 s; compaction lock default 300 s), SHALL reclaim a lease whose expiry passed or whose local holder process is dead, and SHALL return a typed `Busy { holder }` result instead of raising when a lease is held.
**Milestone:** M1

#### Scenario: Dead holder reclaimed
- **GIVEN** a turn lease held by a local process that has exited
- **WHEN** another client acquires the turn lease for that session
- **THEN** the acquisition SHALL succeed without waiting for expiry

### Requirement: Session titles with provenance
The system SHALL sanitise titles (strip control, zero-width, bidi-override and lone-surrogate characters, collapse whitespace, treat empty as null), SHALL reject titles over 100 characters with `Title too long (<n> chars, max 100)`, SHALL keep non-null titles unique per profile, and SHALL rank title provenance `derived` < `llm` < `user` so an automatic title write lands only on an untitled session or one with strictly lower provenance, using compare-and-swap.
**Milestone:** M1

#### Scenario: Auto title does not overwrite user title
- **GIVEN** a session titled `Tax prep` with `title_source = "user"`
- **WHEN** the title generator writes `Spreadsheet help` with provenance `llm`
- **THEN** the title SHALL remain `Tax prep`

#### Scenario: Duplicate title
- **GIVEN** a session titled `Refactor`
- **WHEN** another session is renamed `Refactor` by the user
- **THEN** the system SHALL fail with `Title 'Refactor' is already in use by session <id>`

### Requirement: Title lineage numbering and resolution
The system SHALL name continuations and forks of a titled session `<base> #<n>` with `n` one greater than the highest existing number (the unnumbered original counting as #1), and SHALL resolve a session reference given as an id, an unambiguous id prefix of at least 8 characters, or a title, preferring the most recent `#n` holder for a bare base title.
**Milestone:** M1

#### Scenario: Resume by base title
- **GIVEN** sessions `Refactor` and `Refactor #2`
- **WHEN** the user runs `prometheus --resume Refactor`
- **THEN** the system SHALL resume `Refactor #2`

### Requirement: Full-text session search
The system SHALL provide `search(query, filters)` defaulting to `limit = 20`, `offset = 0`, BM25 rank order (with `sort = newest|oldest` alternatives), filters for `source`, excluded sources, `role`, time range, `session_id`, `pinned` and `include_archived` (default true), SHALL exclude rewound rows (`active = 0, compacted = 0`) unless `include_inactive = true`, SHALL include compaction-archived rows, SHALL route queries containing CJK characters or under 3 characters per term to a trigram index, and SHALL return each hit with session id, message id, role, timestamp and a snippet of at most 200 characters with match markers.
**Milestone:** M1

#### Scenario: Rewound message hidden
- **GIVEN** a message containing `kubernetes` that was rewound
- **WHEN** the user searches `kubernetes`
- **THEN** that message SHALL NOT appear unless `include_inactive = true`

#### Scenario: CJK query
- **WHEN** the user searches `設定ファイル`
- **THEN** the system SHALL answer from the trigram index and return matching messages

### Requirement: Search query sanitisation
The system SHALL cap queries at 2048 characters, preserve balanced quoted phrases, replace unbalanced quotes and index-syntax characters with spaces, drop leading wildcards and leading or trailing boolean operators, and quote terms containing `.`, `-` or `_` so that no user input can produce an index syntax error.
**Milestone:** M1

#### Scenario: Hyphenated term
- **WHEN** the user searches `chat-send`
- **THEN** the system SHALL search for the phrase `"chat-send"` and SHALL NOT return a syntax error

### Requirement: Derived indexes fail open and rebuild offline
The system SHALL treat search indexes as derived: on index corruption it SHALL record `store_meta.index_stale = 1`, disable index triggers in the same transaction, complete the canonical write, and serve searches from a canonical-row scan until `prometheus sessions repair --indexes` rebuilds the indexes; it SHALL NOT rebuild an index on the live write path.
**Milestone:** M1

#### Scenario: Corrupt word index does not lose a message
- **GIVEN** a corrupted word index
- **WHEN** a message is appended
- **THEN** the message SHALL be stored, `index_stale` SHALL be set, and a later search SHALL still find it through the canonical scan

### Requirement: Integrity check and quarantine
The system SHALL run `PRAGMA quick_check` when a store is first opened writable in a process, and on failure or on a structural corruption error at any time SHALL stop writing to the file, move it and its sidecars to `<profile>/quarantine/state-<timestamp>.db`, salvage every readable canonical row into a fresh `state.db` (rebuilding indexes), and report the salvaged and lost row counts at ERROR level and in `prometheus doctor`; it SHALL NOT attempt in-place repair of the corrupt file.
**Milestone:** M1

#### Scenario: Corrupt file quarantined and salvaged
- **GIVEN** a `state.db` whose `quick_check` fails on one page
- **WHEN** the agent starts
- **THEN** the original file SHALL be under `quarantine/`, a new `state.db` SHALL contain every readable session and message, and `prometheus doctor` SHALL report the quarantine path and lost row count

### Requirement: Out-of-band file replacement detection
The system SHALL record the store file's identity (device and inode, or Windows file id, plus `store_meta.store_uuid`) at open and re-check it before every write transaction, refusing writes with error `store.replaced` when the file was replaced underneath the handle, and SHALL reopen only on an explicit new open.
**Milestone:** M1

#### Scenario: File restored from backup while running
- **GIVEN** a running gateway with an open store
- **WHEN** the user copies a backup over `state.db`
- **THEN** the next write SHALL fail with `store.replaced` and SHALL NOT modify the restored file

### Requirement: Write contention policy
The system SHALL perform writes in `BEGIN IMMEDIATE` transactions with a 1 s busy timeout and, on busy or locked results, SHALL retry the whole transaction with random jitter (20-150 ms during the first 2 s, then 250 ms-1 s) until a time budget of 20 s (metadata), 60 s (transcript) or 0.5 s (activity heartbeat) is exhausted, then fail with error `store.busy` naming the budget.
**Milestone:** M1

#### Scenario: Activity write gives up fast
- **GIVEN** another process holding the write lock for 3 s
- **WHEN** an activity heartbeat write is attempted
- **THEN** it SHALL fail with `store.busy` after about 0.5 s without blocking the turn

### Requirement: WAL mode, bounded readers and checkpoints
The system SHALL use WAL journal mode (refusing WAL and failing with `store.unsupported_filesystem` on network or cross-VM filesystems unless `storage.journal_mode = "delete"`), SHALL serve reads from a pool of at most 8 read-only connections per store, and SHALL run a passive checkpoint after every 1000 committed write transactions and on close, using a truncating checkpoint only after `prometheus sessions vacuum`.
**Milestone:** M1

#### Scenario: Readers do not block the writer
- **GIVEN** 8 concurrent dashboard session-list reads
- **WHEN** the agent appends a message
- **THEN** the append SHALL commit without waiting for the reads to finish

### Requirement: Rewind is a soft delete
The system SHALL implement rewind to a user message by setting `active = 0` on that message and every later active message of the session, SHALL fail with `store.rewind_target_invalid` when the target is missing or not a `user` message, SHALL refuse while another holder owns the turn lease, SHALL verify caller-supplied expected message ids inside the transaction, and SHALL increment the session's `rewind_count`.
**Milestone:** M1

#### Scenario: Undo last turn
- **GIVEN** a session whose last user message is `msg_X` followed by 3 assistant/tool messages
- **WHEN** the user runs `/undo`
- **THEN** `msg_X` and the 3 later messages SHALL be inactive, remain in the database, and be excluded from the next model request

### Requirement: Session forking API (upgrade)
The system SHALL provide UI-agnostic operations `session.fork(session_id, at_message_id, title?)`, `session.tree(session_id)` and `session.siblings(session_id)` (exposed identically over the CLI, gateway RPC and dashboard), where a fork creates a new session with `lineage_kind = "fork"` whose active transcript is the parent's active messages up to and including `at_message_id` (copied with new message ids and an `origin_message_id` back-reference), leaving the parent unchanged.
**Milestone:** M2

#### Scenario: Fork from an earlier message
- **GIVEN** session A with active messages m1..m10
- **WHEN** a client calls `session.fork(A, m4)`
- **THEN** a new session F SHALL exist with 4 active messages whose `origin_message_id` values are m1..m4, `lineage_kind = "fork"`, title `<A title> #2`, and A SHALL still have 10 active messages

#### Scenario: Tree lists forks and continuations
- **GIVEN** session A with a fork F and a compression continuation B
- **WHEN** a client calls `session.tree(A)`
- **THEN** the result SHALL contain F with kind `fork` and B with kind `compression`

### Requirement: Usage events keyed by root lineage
The system SHALL store model usage only in the append-only `usage_ledger` table whose columns are defined by `observability` (including `session_id`, `root_session_id`, provider, model, input/output/cache-read/cache-write/reasoning tokens, `cost_usd`, `cost_status` and `billing_source`), keyed by `root_session_id`, and SHALL compute session, lineage and profile totals by summing rows over `session_id`, `root_session_id` or profile respectively, so that compression continuations, forks and delegated children are never omitted from lineage totals.
**Milestone:** M1

#### Scenario: Continuation usage counted in lineage total
- **GIVEN** session A used 1000 input tokens, then compressed into B which used 500
- **WHEN** lineage usage for A is requested
- **THEN** the total SHALL be 1500 input tokens

### Requirement: Resume and export size guards
The system SHALL refuse to resume a session whose active lineage exceeds `sessions.max_resume_messages` (default 20000; 0 disables) with error `session.too_large_to_resume` suggesting `/compress` or a fork, and SHALL stream exports of any size to a file while refusing in-memory exports above `sessions.max_export_messages` (default 20000) with `session.too_large_to_export`.
**Milestone:** M1

#### Scenario: Oversized resume refused
- **GIVEN** a lineage with 25000 active messages and default config
- **WHEN** the user resumes it
- **THEN** the system SHALL fail with `session.too_large_to_resume` and SHALL NOT load the transcript

### Requirement: Deletion semantics
The system SHALL delete a session together with its messages, usage ledger rows and unreferenced blobs, cascade to `delegate` children, re-parent `fork` and `reset` children to a null parent (marking them `lineage_kind` preserved with `parent_deleted = 1`), delete `compression` continuations only when `--with-lineage` is given, and SHALL refuse with `session.in_use` while a turn lease or compaction lock is live.
**Milestone:** M1

#### Scenario: Deleting a forked parent keeps the fork
- **GIVEN** session A with fork F
- **WHEN** A is deleted
- **THEN** F SHALL remain readable with `parent_deleted = 1`

### Requirement: Retention, pruning and auto-archive
The system SHALL prune only ended sessions inactive for longer than `sessions.retention_secs` (default 0, meaning never) during maintenance at most once per 24 hours, SHALL never prune a pinned session or a session with a live lease, SHALL prune a compression ancestor only together with its whole chain, and SHALL mark lineage tips idle for `sessions.auto_archive_secs` (default 604800; 0 disables) as `archived = "auto"`, which resume clears; `prometheus sessions prune --older-than <days>` SHALL apply the same rules on demand and print the count it would delete under `--dry-run`.
**Milestone:** M1

#### Scenario: Pinned session survives retention
- **GIVEN** `sessions.retention_secs = 2592000` and a pinned session ended 100 days ago
- **WHEN** maintenance runs
- **THEN** the pinned session SHALL NOT be deleted

### Requirement: Portable export and import
The system SHALL export sessions as JSONL (`prometheus sessions export [<ref>...|--all] [--include-inactive] -o FILE`) whose first line is a header `{"format":"prometheus.sessions","version":1}` followed by one session record and its messages, usage events and lineage per line, with blobs embedded base64 under `--with-blobs`; import SHALL validate the whole file before writing, SHALL be all-or-none, SHALL refuse id collisions unless `--remap-ids` assigns new ids (preserving lineage), and SHALL bound input to 500 sessions, 50000 messages and 256 MiB per file.
**Milestone:** M2

#### Scenario: Collision without remap
- **GIVEN** an export containing a session id that already exists
- **WHEN** it is imported without `--remap-ids`
- **THEN** the import SHALL fail with `import.id_collision` listing the ids and SHALL write nothing

### Requirement: Session management CLI
The system SHALL provide `prometheus sessions` subcommands `list`, `show <ref>`, `search <query>`, `rename <ref> <title>`, `pin`, `unpin`, `archive`, `unarchive`, `delete`, `fork <ref> [--at <msg>]`, `tree <ref>`, `prune`, `export`, `import`, `stats`, `repair [--indexes]` and `vacuum`, each accepting `--json` for machine-readable output and operating read-only where the operation does not mutate.
**Milestone:** M1

#### Scenario: JSON listing
- **WHEN** the user runs `prometheus sessions list --json --limit 5`
- **THEN** stdout SHALL be a JSON array of at most 5 session objects containing `id`, `title`, `source`, `started_at` and `message_count`

### Requirement: Hybrid semantic and full-text search (upgrade)
The system SHALL, when `sessions.search.semantic = true` (default false), embed active user and assistant messages in the background (never inside a turn) using the profile's auxiliary embedding model into a derived `message_embeddings` index, and answer `search(..., mode = "hybrid")` by fusing full-text and vector rankings with reciprocal rank fusion (k = 60), falling back to full-text only with a `degraded: "semantic_unavailable"` marker when embeddings are missing or the embedding model is unreachable.
**Milestone:** M3

#### Scenario: Paraphrase found
- **GIVEN** semantic search enabled and a message `We moved the database to Postgres 16`
- **WHEN** the user searches `which DB engine did we migrate to` in hybrid mode
- **THEN** that message SHALL be among the top 5 results

#### Scenario: Embedding model offline
- **GIVEN** semantic search enabled and the embedding provider unreachable
- **WHEN** a hybrid search runs
- **THEN** the system SHALL return full-text results with `degraded = "semantic_unavailable"`

### Requirement: Encryption at rest option (upgrade)
The system SHALL support `storage.encryption = "none" | "keychain" | "passphrase"` (default `none`), where non-`none` modes encrypt `state.db` pages and blob files with a per-profile 256-bit key held in the OS keychain or derived from a passphrase (Argon2id), SHALL convert an existing store only via the offline command `prometheus sessions encrypt|decrypt` (writing a new file then atomically replacing), and SHALL never write plaintext transcript content to temporary files when encryption is enabled.
**Milestone:** M3

#### Scenario: Store unreadable without key
- **GIVEN** a profile with `storage.encryption = "keychain"`
- **WHEN** `state.db` is opened by a generic SQLite tool
- **THEN** the file SHALL not be readable as a SQLite database

#### Scenario: Missing key fails closed
- **GIVEN** an encrypted store whose keychain entry was removed
- **WHEN** the agent starts
- **THEN** the system SHALL exit with code 78 and the message `state.db is encrypted and its key is unavailable` and SHALL NOT create a new empty store

### Requirement: Integration source tag
The system SHALL store, besides the typed `source`, an optional `source_tag` (`^[a-z][a-z0-9_-]{0,31}$`) set by `prometheus chat --source <tag>` or the RPC session-create parameter, SHALL record one-shot runs with `source = "cli"` and `source_tag = "oneshot"`, and SHALL hide sessions whose tag is `oneshot` or `tool` from interactive pickers, `-c`, `--resume latest` and `/sessions` while keeping them in `sessions list --all`, search and usage totals.
**Milestone:** M1

#### Scenario: Integration session hidden from continue
- **GIVEN** a session created with `--source tool` after the user's last interactive session in the same directory
- **WHEN** the user runs `prometheus -c`
- **THEN** the system SHALL resume the interactive session

### Requirement: Interactive session browser
The system SHALL provide `prometheus sessions browse [--source <s>] [--all]`, an interactive picker listing sessions newest first with title, relative last activity, message count, source and a status tag derived from the session state (`live`, `done` for a final assistant answer, `intr` for `interrupted`, `err` for `error`, `empty` for no messages), incremental search over titles and full-text hits, and Enter resuming the selected session, exiting 2 when stdout is not a terminal.
**Milestone:** M1

#### Scenario: Status tags
- **GIVEN** a session that ended with `end_reason = "interrupted"`
- **WHEN** the user opens `prometheus sessions browse`
- **THEN** its row SHALL carry the tag `intr`

### Requirement: Offline recovery and journal mode
The system SHALL provide `prometheus sessions recover [-o <file>]`, which with the profile's store closed (exit 75 when a daemon or client holds it) copies every readable row into a new clean store, writes it next to the original as `state.recovered-<timestamp>.db`, reports rows recovered and lost per table, and swaps it in only after confirmation keeping the original as `state.db.pre-recover`, and `prometheus sessions journal-mode <wal|delete>` switching the store's journal mode offline (for filesystems without shared-memory support), recorded in `store_meta` and reapplied on open.
**Milestone:** M2

#### Scenario: Recover refused while open
- **GIVEN** a running daemon serving the profile
- **WHEN** the user runs `prometheus sessions recover`
- **THEN** the command SHALL exit 75 naming the daemon pid and change nothing

### Requirement: Prompt refresh and retitling
The system SHALL provide `prometheus sessions refresh-prompts [<ref>...|--all]` which marks resumable sessions so that their next turn rebuilds the stored system prompt from the current profile (identity, skills index, tool set, memory snapshot) keeping the transcript, recording one `prompt.rebuilt` event with `reason` `refresh-prompts`, and `prometheus sessions retitle [<ref>...|--auto-only]` regenerating titles through the auxiliary model only for sessions whose `title_source` is not `user`, and `prometheus sessions list --pinned` listing pinned sessions.
**Milestone:** M2

#### Scenario: User titles kept
- **GIVEN** a session titled by the user and one titled automatically
- **WHEN** the user runs `prometheus sessions retitle --all`
- **THEN** only the automatically titled session SHALL get a new title

### Requirement: Export formats and save command
`prometheus sessions export` SHALL accept `--format jsonl|md|html|prompts|trace` (default `jsonl`), where `md` renders a readable Markdown transcript with tool calls collapsed, `html` a self-contained page with no external resources, `prompts` only the user messages one per entry, and `trace` the shared trajectory JSONL grammar, all exporting only active messages unless `--include-inactive`; the registry SHALL provide `/save [json|md|html] [file] [redact]` writing the current session to `<file>` or `<cwd>/<title-slug>-<YYYYMMDD-HHMM>.<ext>`, applying secret redaction when `redact` is given, and printing usage for a bare `/save`.
**Milestone:** M1

#### Scenario: Redacted save
- **GIVEN** a session whose transcript contains `sk-live-abcdef1234567890`
- **WHEN** the user types `/save md notes.md redact`
- **THEN** `notes.md` SHALL NOT contain that key and SHALL contain a redaction marker

### Requirement: Low-disk prune prompt
The system SHALL, at interactive startup at most once per day, check free space on the volume holding the profile's `state.db` and, when it is below `sessions.low_disk_bytes` (default 1073741824) and `sessions.retention_secs = 0`, offer once to enable auto-prune with answers `on`, `off` (never ask again) or `later` (ask again after 7 days), and SHALL provide `/autoprune [on|off|later|status]` where `on` sets `sessions.retention_secs` to 7776000 (90 days) unless already set and `status` prints the retention setting, store size and free space.
**Milestone:** M2

#### Scenario: Later postpones a week
- **GIVEN** low free disk space and the user answered `later` today
- **WHEN** an interactive session starts tomorrow
- **THEN** the prompt SHALL NOT be shown

### Requirement: Attachment storage location
The system SHALL store session attachments in the profile blob store by default (`attachments.storage = "blobs"`), SHALL accept `attachments.storage = "workspace"` which additionally writes a copy of each received attachment under `<cwd>/.prometheus/attachments/<session_id>/` for tools to read by path, and SHALL keep the blob store as the canonical reference in either mode.
**Milestone:** M2

#### Scenario: Workspace copy
- **GIVEN** `attachments.storage = "workspace"`
- **WHEN** a user sends `report.pdf` in a CLI session in `/repo`
- **THEN** the file SHALL exist under `/repo/.prometheus/attachments/<session_id>/` and in the blob store

### Requirement: Session search tool
The `session_search` tool SHALL accept `query`, `limit` (default 10, maximum 50), `sort` (`relevance` default, `newest`, `oldest`), `after` and `before` (RFC 3339 or `YYYY-MM-DD`), `role_filter` (list of `user`, `assistant`, `tool`), `exclude_session_ids`, `detail` (`adaptive` default, returning snippets plus an auxiliary-model summary per session when more than 3 sessions match; `full` returning raw snippets) and a window mode (`session_id`, `around_message_id`, `window` default 5, maximum 25) returning the active messages around a hit, SHALL search only the calling session's profile, SHALL exclude the calling session's own active context by default, and SHALL cap the tool result at 16 KiB.
**Milestone:** M1

#### Scenario: Window around a hit
- **GIVEN** a hit `msg_X` in session `ses_Y`
- **WHEN** the model calls `session_search` with `session_id = "ses_Y"`, `around_message_id = "msg_X"` and `window = 2`
- **THEN** the result SHALL contain at most the 2 active messages before and after `msg_X` plus `msg_X`

#### Scenario: Profile isolation
- **GIVEN** a matching message in profile `home`
- **WHEN** a session in profile `work` calls `session_search`
- **THEN** the result SHALL NOT include the `home` message
