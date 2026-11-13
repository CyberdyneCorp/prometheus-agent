// Generated from schema/rpc.json by `cargo xtask gen`; do not edit.

export const PROTOCOL = { major: 1, minor: 0 } as const;

export const ERROR_CODES = {
  "auth": -32001,
  "permission_denied": -32002,
  "not_found": -32003,
  "conflict": -32004,
  "budget_exceeded": -32005,
  "unavailable": -32006,
  "cancelled": -32007,
  "protocol": -32008,
  "invalid_input": -32010,
  "provider": -32011,
  "tool": -32012,
  "sandbox": -32013,
  "config": -32014,
  "internal": -32099,
  "invalid_request": -32600,
  "unknown_method": -32601,
  "invalid_params": -32602,
  "parse_error": -32700,
} as const;

export const CLOSE_CODES = {
  "normal": 1000,
  "shutting_down": 1001,
  "internal": 1011,
  "bad_credential": 4401,
  "not_allowed": 4403,
  "endpoint_disabled": 4404,
  "heartbeat_timeout": 4408,
  "session_ended": 4410,
  "frame_too_large": 4413,
  "protocol_unsupported": 4426,
} as const;

/** `{"ok": true}`. */
export interface Ack {
  /** Always true. */
  ok: boolean;
}

/** `adapter.state` payload: one messaging adapter changed state (spec: platform-adapters, Adapter state reporting and reconnect). */
export interface AdapterStatePayload {
  /** Outside `connected` longer than `gateway.reconnect_attention_secs` (set once per outage, cleared on reconnect). */
  attention?: boolean;
  /** Error code of a `degraded`, `retrying` or `fatal` adapter. */
  code?: string | null;
  /** What happened. */
  message?: string | null;
  /** Platform name. */
  platform: string;
  /** The owning profile. */
  profile: string;
  /** `disabled`, `connecting`, `connected`, `degraded`, `retrying` or `fatal`. */
  state: string;
}

/** How `prompt.submit` admitted an input (spec: tui, Prompt submission while busy). */
export type Admission = "started" | "queued" | "steered" | "handled";

/** The answer to `approval.request` (also `approval.respond` params, with its `id`). */
export interface ApprovalAnswer {
  /** For a diff approval: the hunks accepted. */
  accepted_hunks?: Array<number> | null;
  /** The decision. */
  decision: Decision;
  /** An edited command to run instead. */
  replacement?: string | null;
}

/** Why a call needs approval. */
export interface ApprovalFinding {
  /** The detector (`dangerous-pattern`, `protected-file`, ...). */
  detector: string;
  /** The offending part, invisible characters escaped. */
  fragment: string;
  /** What it found. */
  reason: string;
}

/** `approval.request` params. */
export interface ApprovalRequestParams {
  /** The model's call id. */
  call_id: string;
  /** Milliseconds until the request settles unanswered (set by the daemon when it sends the request). */
  deadline_ms?: number | null;
  /** What an `explain` answer produced: the parsed command, the rule, the paths it touches and a dry-run preview (the request is asked again with it). */
  explanation?: string | null;
  /** Why approval is needed. */
  findings: Array<ApprovalFinding>;
  /** What `session` and `always` record. */
  remember?: string | null;
  /** The session. */
  session_id: string;
  /** The command line or written path. */
  subject?: string | null;
  /** The tool called. */
  tool: string;
}

/** `approval.respond` params. */
export interface ApprovalRespondParams {
  /** For a diff approval: the hunks accepted. */
  accepted_hunks?: Array<number> | null;
  /** The decision. */
  decision: Decision;
  /** The `approval.request` id (`srq-<12 hex>`). */
  id: string;
  /** An edited command to run instead. */
  replacement?: string | null;
}

/** One image, file or link a session produced. */
export interface Artifact {
  /** The tool call that wrote or produced it. */
  call_id?: string | null;
  /** When (RFC 3339). */
  created_at: string;
  /** Stable id (`<message id>:<n>`). */
  id: string;
  /** `image`, `file` or `link`. */
  kind: string;
  /** The session that produced it. */
  session_id: string;
  /** Its title (empty when untitled). */
  session_title: string;
  /** That call's tool. */
  tool?: string | null;
  /** The path (as the session names it) or the URL. */
  value: string;
}

/** `artifacts.list` result. */
export interface ArtifactList {
  /** Newest first. */
  artifacts: Array<Artifact>;
  /** The next page's cursor, while there is one. */
  next_cursor?: string | null;
}

/** `artifacts.list` params. */
export interface ArtifactsListParams {
  /** Where the previous page ended. */
  cursor?: string | null;
  /** The profile (required when the connection is granted several). */
  profile?: string | null;
  /** Only artifacts whose value or session title contains this text. */
  query?: string | null;
  /** Only this session's artifacts (the composer's suggestions). */
  session_id?: string | null;
}

/** `session.attach` params. */
export interface AttachParams {
  /** The daemon epoch `last_seen_seq` belongs to; another epoch than the daemon's answers `truncated: true` and replays nothing. */
  epoch?: string | null;
  /** The newest sequence number the client applied: the kept events after it are replayed before the answer. */
  last_seen_seq?: number | null;
  /** Driver (default) or observer. */
  role?: AttachRole;
  /** `ses_<ULID>`. */
  session_id: string;
}

/** What an attached connection may do in the session. */
export type AttachRole = "driver" | "observer";

/** `session.attach` / `session.resume` result. */
export interface Attached {
  /** The daemon epoch the sequence numbers belong to. */
  epoch: string;
  /** The newest event sequence number kept for it. */
  latest_seq: number;
  /** Requests of the session still waiting for an answer (re-sent to this connection when it is a capable driver). */
  open_requests?: Array<OpenRequest>;
  /** The session. */
  session_id: string;
  /** Events after the client's `last_seen_seq` were evicted, or the client's epoch is not this daemon's: reload the transcript (`session.transcript`) instead of applying the replayed events. */
  truncated?: boolean;
}

/** One kind of a session's automation. */
export interface AutomationStatePayload {
  /** Turns (goal), wakeups (loop) or fires (heartbeat) so far. */
  count?: number;
  /** `goal`, `loop` or `heartbeat`. */
  kind: string;
  /** A goal's turn budget (`goals.max_turns`). */
  limit?: number | null;
  /** When it fires next (RFC 3339), for an active loop or heartbeat or a parked goal's deadline. */
  next_fire_at?: string | null;
  /** The live row as the CLI shows it (absent once removed). */
  row?: string | null;
  /** The session. */
  session_id: string;
  /** `active`, `paused`, `done`, `parked`, or `none` once removed. */
  status: string;
}

/** `autoprune.offer` result. */
export interface AutopruneOffer {
  /** The question to ask (`on`, `off` or `later`, answered with `/autoprune <answer>`), when it is due; `null` otherwise. A non-null answer counts as today's check. */
  question?: string | null;
}

/** `autoprune.offer` params. */
export interface AutopruneOfferParams {
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** `session.banner` result: what the TUI's startup banner lists. */
export interface BannerInfo {
  /** The degraded marker while the session runs below full hardening, for example `sandbox: partial:landlock_net` (spec: security-boundaries, Hardening downgrades are never silent). */
  degraded?: string | null;
  /** The session's MCP servers. */
  mcp_servers: Array<McpServerState>;
  /** The installed skills the session can load. */
  skills: Array<string>;
  /** The frozen system prompt; none before the first turn. */
  system_prompt?: FrozenPrompt | null;
  /** The session's tool names, in request order. */
  tools: Array<string>;
}

/** `cron.blueprints.fill` params. */
export interface BlueprintFillParams {
  /** Save the job (after the caller showed the echo); otherwise only check and echo. */
  create?: boolean;
  /** The blueprint (key, prefix or near spelling). */
  key: string;
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
  /** Slot values by name. */
  values?: Record<string, string>;
}

/** `cron.blueprints.fill` result. */
export interface BlueprintFilled {
  /** The schedule echo: the canonical form and the next instants. */
  echo?: string | null;
  /** The created job's id. */
  job_id?: string | null;
  /** The blueprint's key. */
  key: string;
  /** The slot that needs a value. */
  missing?: string | null;
  /** The job's prompt. */
  prompt?: string | null;
  /** Its question. */
  question?: string | null;
  /** The canonical schedule (`cron 0 8 * * 1-5`). */
  schedule?: string | null;
  /** `missing` (a slot needs a value), `ready` (echoed, not saved) or `created`. */
  status: string;
}

/** One blueprint. */
export interface BlueprintInfo {
  /** Its category. */
  category: string;
  /** What it does. */
  description: string;
  /** Its key (`<plugin>:<key>`, `skill:<key>` outside the built-ins). */
  key: string;
  /** Its slots, in the order they are asked. */
  slots: Array<BlueprintSlot>;
  /** `builtin`, `plugin` or `skill`. */
  source: string;
  /** Its title. */
  title: string;
}

/** `cron.blueprints.list` result. */
export interface BlueprintList {
  /** The profile's catalog. */
  blueprints: Array<BlueprintInfo>;
}

/** One slot, as a form field. */
export interface BlueprintSlot {
  /** The value used when none is given. */
  default?: string | null;
  /** `time`, `enum`, `text` or `weekdays`. */
  kind: string;
  /** The field name (`time`, `days`...). */
  name: string;
  /** The options of an `enum`; the presets of `weekdays`. */
  options: Array<string>;
  /** The question asked for it. */
  question: string;
  /** Needs a value (given or default). */
  required: boolean;
  /** An `enum` accepts only its options. */
  strict: boolean;
}

/** `cron.blueprints.list` params. */
export interface BlueprintsListParams {
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** `bot.chat.ensure` result. */
export interface BotChat {
  /** Whether this call created it. */
  created: boolean;
  /** The profile's canonical Bot Chat. */
  session_id: string;
}

/** One budget's standing in `usage.updated`. */
export interface BudgetStanding {
  /** The limit. */
  limit: number;
  /** `limit - spent`, never below zero. */
  remaining: number;
  /** `turn`, `session`, `root`, `daily`, `monthly` or `unattended`. */
  scope: string;
  /** Spent in the scope's period. */
  spent: number;
  /** `usd` or `tokens`. */
  unit: string;
}

/** `clarify.request` result. */
export interface ClarifyAnswers {
  /** One answer per question, as the `clarify` tool defines them. */
  answers: Array<unknown>;
}

/** `clarify.request` params. */
export interface ClarifyRequestParams {
  /** Surfaces render a free-text `Other` option. */
  allow_other: boolean;
  /** Milliseconds until the request settles unanswered (set by the daemon when it sends the request). */
  deadline_ms?: number | null;
  /** The questions, as the `clarify` tool defines them. */
  questions: Array<unknown>;
  /** The session. */
  session_id: string;
}

/** What the client can handle. */
export interface ClientCapabilities {
  /** Events introduced after the client's minor version that it understands anyway. */
  events?: Array<string>;
  /** The client answers server-to-client requests (approvals, clarify, secrets, sudo). Without it they resolve unanswered at once. */
  server_requests?: boolean;
}

/** Which client is attaching. */
export type ClientKind = "cli" | "tui" | "desktop" | "dashboard" | "acp" | "eval";

/** `commands.catalog` result. */
export interface CommandCatalog {
  /** Every command of the session, by name. */
  commands: Array<CommandEntry>;
}

/** One `commands.catalog` entry. */
export interface CommandEntry {
  /** Other names that resolve to it. */
  aliases?: Array<string>;
  /** The argument synopsis (`<name> [--now]`), when it takes any. */
  args?: string | null;
  /** One line. */
  description: string;
  /** Whether it runs from the asking client's surface. */
  disposition: Disposition;
  /** The name without `/`. */
  name: string;
  /** Why it does not run there. */
  reason?: string | null;
  /** Built-in, quick command, skill or client-handled. */
  source: CommandSource;
  /** Subcommands with their own disposition on this surface (a palette offers a command when it or one of them runs here). */
  subcommands?: Array<SubcommandEntry>;
}

/** Where a command comes from. */
export type CommandSource = "builtin" | "quick" | "skill" | "client";

/** `session.compact` params. */
export interface CompactParams {
  /** What the summary should keep in focus. */
  focus?: string | null;
  /** The session. */
  session_id: string;
}

/** `session.compact` result. */
export interface Compacted {
  /** How the compaction ended. */
  status: string;
  /** Context tokens after. */
  tokens_after: number;
  /** Context tokens before. */
  tokens_before: number;
}

/** `complete.slash` / `complete.path` params. */
export interface CompleteParams {
  /** What is typed so far (`/mod` for a command, `./src/ma` for a path, without the `@` of a mention). */
  prefix: string;
  /** The session. */
  session_id: string;
}

/** One completion. */
export interface Completion {
  /** What it is (a command's description). */
  description?: string | null;
  /** A directory (path completion continues inside it). */
  dir?: boolean;
  /** The completed text that replaces the prefix. */
  text: string;
}

/** `complete.slash` / `complete.path` result. */
export interface Completions {
  /** The candidates, best first. */
  items: Array<Completion>;
}

/** One component's health. */
export interface ComponentHealth {
  /** `store`, `scheduler`, `adapter:<platform>`, `mcp:<server>`... */
  component: string;
  /** The owning profile; `null` for daemon-wide components. */
  profile?: string | null;
  /** Why. */
  reason: string;
  /** When it entered the state (RFC 3339). */
  since: string;
  /** `ok`, `degraded` or `down`. */
  state: string;
}

/** One change of a `config.patch`. */
export interface ConfigChange {
  /** A dotted key (`delegation.model`). */
  key: string;
  /** The new value (typed by the key's schema); `null` or absent removes the key so its default applies. */
  value?: unknown;
}

/** `config.get` params. */
export interface ConfigGetParams {
  /** A dotted key (`model.default`). */
  key: string;
  /** The profile; required when the connection is granted several. */
  profile?: string | null;
}

/** `config.patch` params. */
export interface ConfigPatchParams {
  /** Applied together, validated as a whole: all or none. */
  changes: Array<ConfigChange>;
  /** The window's profile. */
  profile: string;
}

/** `config.patch` result. */
export interface ConfigPatched {
  /** The keys the patch named. */
  keys: Array<string>;
}

/** `config.reload` result: the profile's configuration state. */
export interface ConfigReloaded {
  /** Why the file on disk is not in use, while it is invalid. */
  error?: string | null;
  /** Keys that apply after a daemon restart. */
  pending_restart?: Array<string>;
  /** The profile. */
  profile: string;
  /** Valid edits loaded since the daemon started. */
  reloads: number;
}

/** `config.get` result. */
export interface ConfigValue {
  /** The key. */
  key: string;
  /** The layer it comes from (`default`, `file`, `set`, `managed`). */
  source: string;
  /** Its effective value (secrets are never configuration values). */
  value: unknown;
}

/** One category of `session.context_usage`. */
export interface ContextCategory {
  /** `system_prompt`, `tool_definitions`, `skills`, `memory`, `rules`, `mcp`, `subagent_definitions` or `conversation`. */
  category: string;
  /** Its estimated tokens. */
  tokens: number;
}

/** `session.context_usage` result. */
export interface ContextUsage {
  /** The current route's `billing_source` (`subscription:<provider>/<label>`, `api_key:<provider>[/<label>]`, `local`...), when the session is loaded. */
  billing_source?: string | null;
  /** `tokens` by category; they sum to `tokens` (empty when the session is not loaded). */
  categories?: Array<ContextCategory>;
  /** The model's context window, when known. */
  context_window?: number | null;
  /** The session's cost so far in the usage ledger's currency (USD), priced calls only. */
  cost_usd?: number | null;
  /** The subscription the current route bills, when it is one. */
  subscription?: SubscriptionStatus | null;
  /** Estimated tokens of the active context. */
  tokens: number;
}

/** An approval answer. */
export type Decision = "once" | "session" | "always" | "deny" | "explain";

/** `command.dispatch` params: a slash command the client does not handle itself (spec: tui, Slash command flow). */
export interface DispatchParams {
  /** The rest of the line, case kept. */
  args?: string;
  /** The command name without `/` (an alias or unique prefix resolves). */
  name: string;
  /** The session. */
  session_id: string;
}

/** `command.dispatch` result. */
export type Dispatched = {
  /** What to show (may be empty). */
  notice: string;
  type: "done";
} | {
  /** Images that go with it. */
  images?: Array<PromptImage>;
  /** The prompt. */
  message: string;
  type: "send";
} | {
  /** Why. */
  reason: string;
  type: "refused";
};

/** Where a command runs from this surface. */
export type Disposition = "exec" | "unavailable" | "hidden";

/** `egress.set` params: changes applied in order (mode, then allow, then deny); an empty change only reads the policy. */
export interface EgressSetParams {
  /** Host patterns to admit (`host`, `host:port`, `*.suffix`). */
  allow?: Array<string>;
  /** Host patterns to refuse in every mode. */
  deny?: Array<string>;
  /** `public`, `allowlist` or `none`. */
  mode?: string | null;
  /** The session. */
  session_id: string;
}

/** `egress.set` result: the session's policy after the change. */
export interface EgressState {
  /** Admitted patterns. */
  allow: Array<string>;
  /** Refused patterns. */
  deny: Array<string>;
  /** The mode. */
  mode: string;
}

/** No params. */
export type Empty = Record<string, unknown>;

/** One event frame: the notification params and the replay item. */
export interface Envelope {
  /** The payload, as the event's contract defines it. */
  data: unknown;
  /** The daemon epoch. */
  epoch: string;
  /** The event name. */
  event: string;
  /** Per-session monotonic sequence number (per daemon for daemon-wide events). */
  seq: number;
  /** The session it belongs to; `None` for daemon-wide events. */
  session_id?: string | null;
}

/** `session.events.since` result. */
export interface EventsSince {
  /** The daemon epoch; a different one than the client's means the daemon restarted and the client resets its watermark. */
  epoch: string;
  /** The kept events after `last_seen`, oldest first. */
  events: Array<Envelope>;
  /** The newest sequence number of the session. */
  latest_seq: number;
  /** Requests of the session still waiting for an answer. */
  open_requests: Array<OpenRequest>;
  /** Events after `last_seen` were evicted. */
  truncated: boolean;
}

/** `session.events.since` params. */
export interface EventsSinceParams {
  /** The last sequence number the client saw (0 for none). */
  last_seen: number;
  /** The session. */
  session_id: string;
}

/** One row of the platform support matrix. */
export interface FeatureSupport {
  /** The feature (`sandbox.local`, `containers`...). */
  feature: string;
  /** What the probe found: the mechanism in use, or what is missing and whether the operating system or a dependency lacks it. */
  reason: string;
  /** `supported`, `degraded` or `unsupported`. */
  state: string;
}

/** `files.rollback` params: no checkpoint lists them; `diff` previews. */
export interface FilesRollbackParams {
  /** Overwrite hand-edited files too. */
  all?: boolean;
  /** The checkpoint, 1 for the newest. */
  checkpoint?: number | null;
  /** Show the diff to the current state; change nothing. */
  diff?: boolean;
  /** Restore only this file. */
  file?: string | null;
  /** The session (its workspace). */
  session_id: string;
}

/** `files.rollback` result. */
export interface FilesRolledBack {
  /** The listing, the diff or what was restored, as text. */
  output: string;
}

/** `files.undo` params. */
export interface FilesUndoParams {
  /** Report only; change nothing. */
  dry_run?: boolean;
  /** Overwrite files changed since the session wrote them. */
  force?: boolean;
  /** The session. */
  session_id: string;
  /** How many of its last turns (default 1). */
  turns?: number | null;
}

/** `files.undo` result. */
export interface FilesUndone {
  /** `conflict: <path> changed since turn <n>; use --force to overwrite`. */
  conflicts: Array<string>;
  /** Files the session had created, removed. */
  removed: Array<string>;
  /** Files given their prior content. */
  restored: Array<string>;
  /** The turns undone. */
  turns: Array<number>;
}

/** `session.fork` params. */
export interface ForkParams {
  /** The last message the fork keeps (`msg_<ULID>`); default the session's last active message. */
  at_message_id?: string | null;
  /** The session to fork. */
  session_id: string;
  /** The fork's title; default `<parent title> #<n>`. */
  title?: string | null;
}

/** The system prompt a session froze on its first turn. */
export interface FrozenPrompt {
  /** `sha256:<hex>` of the prompt text. */
  sha256: string;
  /** The prompt text, its tiers joined in request order. */
  text: string;
}

/** One listed entry. */
export interface FsEntry {
  /** A directory (expanded lazily with another `fs.list`). */
  dir: boolean;
  /** Ignored by a `.gitignore` (listed only with `show_ignored`). */
  ignored?: boolean;
  /** The file name. */
  name: string;
  /** Its path as the session names it (relative to the workspace when inside it). */
  path: string;
  /** Its size in bytes, for a file. */
  size?: number | null;
}

/** `fs.read` result. */
export interface FsFile {
  /** Where the file is on this machine, when the session's backend is the host's filesystem (the guarded opener may open it). */
  absolute?: string | null;
  /** The bytes read, base64. */
  data: string;
  /** `text`, `image` or `binary`. */
  kind: string;
  /** The media type of an image (`image/png`...). */
  mime?: string | null;
  /** The file as the session names it. */
  path: string;
  /** Its whole size in bytes. */
  size: number;
  /** Only the first `max_bytes` are in `data`. */
  truncated: boolean;
  /** The session's workspace on this machine, with `absolute`. */
  workspace?: string | null;
}

/** `fs.list` params. */
export interface FsListParams {
  /** A directory, relative to the session's workspace (`.` its root). */
  path?: string;
  /** The session (attached). */
  session_id: string;
  /** Also list entries `.gitignore` files ignore. */
  show_ignored?: boolean;
}

/** `fs.list` result. */
export interface FsListing {
  /** Directories first, then files, by name. */
  entries: Array<FsEntry>;
  /** The listed directory. */
  path: string;
}

/** `fs.read` params. */
export interface FsReadParams {
  /** Read at most this many bytes from the start (default and cap 8 MiB); the pane previews the first 2 MiB. */
  max_bytes?: number | null;
  /** The file, relative to the session's workspace or absolute. */
  path: string;
  /** The session (attached). */
  session_id: string;
}

/** `health.changed` payload. */
export interface HealthChanged {
  /** The component. */
  component: string;
  /** The previous state (`null` when first reported). */
  previous?: string | null;
  /** Its profile; `null` for daemon-wide components. */
  profile?: string | null;
  /** Why. */
  reason: string;
  /** When (RFC 3339). */
  since: string;
  /** The new state. */
  state: string;
}

/** `rpc.hello` params: the first request on every connection. */
export interface HelloParams {
  /** What it can handle. */
  capabilities?: ClientCapabilities;
  /** The attaching client. */
  client: ClientKind;
  /** Its version. */
  client_version: string;
  /** The protocol it speaks. */
  protocol: Version;
}

/** `rpc.hello` result. */
export interface HelloResult {
  /** The daemon's build id; a client with another one shows the stale-build notice. */
  build_id: string;
  /** What the daemon serves. */
  capabilities: ServerCapabilities;
  /** Random per daemon start; event sequence numbers restart with it. */
  epoch: string;
  /** Random per daemon start; tickets are bound to it. */
  instance_id: string;
  /** Hash of the daemon's `schema/rpc.json`. */
  methods_digest: string;
  /** The daemon's target and tier. */
  platform_support: PlatformSupport;
  /** The profiles the connection's ticket grants. */
  profiles: Array<string>;
  /** The protocol the daemon speaks. */
  protocol: Version;
  /** The daemon's semantic version. */
  server_version: string;
}

/** `journey.list` result. */
export interface JourneyList {
  /** The nodes, oldest first. */
  nodes: Array<JourneyNode>;
}

/** `journey.list` params. */
export interface JourneyListParams {
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
  /** Only nodes created at or after this: a duration back from now (`2h`, `3d`, `1w`), a date (`2026-11-01`) or a timestamp. */
  since?: string | null;
}

/** One learned skill or memory entry. */
export interface JourneyNode {
  /** When it was learned (RFC 3339). */
  created_at: string;
  /** `skill:<name>` for a skill, the entry id (`m_...`) for memory. */
  id: string;
  /** `skill` or `memory`. */
  kind: string;
  /** Who created it: a skill's `user`, `agent`, `hub` or `bundled` (a used bundled skill); an entry's `user`, `agent`, `background_review`, `provider`, `consolidation` or `import`. */
  origin: string;
  /** The skill's name or the entry's text (one line, shortened). */
  title: string;
  /** When it last changed (RFC 3339). */
  updated_at: string;
}

/** `session.tree` / `session.siblings` result. */
export interface Lineage {
  /** The related sessions, by depth then age. */
  nodes: Array<LineageNode>;
}

/** One related session in `session.tree` / `session.siblings`. */
export interface LineageNode {
  /** Distance from the asked session (its children are 1). */
  depth: number;
  /** How it relates to the parent: `fork`, `compression`, `delegate`, `reset`, `import` or `background`. */
  kind: string;
  /** Its parent. */
  parent_session_id: string;
  /** The session. */
  session_id: string;
  /** Its title. */
  title?: string | null;
}

/** Params of every `local.*` method; each uses the fields its command takes (`model` for pull, rm and use; `job` for pause and resume; `query` for search; `path` for add). */
export interface LocalParams {
  /** A download job id. */
  job?: string | null;
  /** Catalog or library id, or `owner/repo[:quant]` for `local.pull`. */
  model?: string | null;
  /** A `.gguf` file on the daemon's machine. */
  path?: string | null;
  /** The profile (the connection's only one when omitted); `local.use` sets its model. */
  profile?: string | null;
  /** Search words. */
  query?: string | null;
}

/** Result of every `local.*` method. */
export interface LocalReply {
  /** The command's `--json` document (without `schema`), or `null`. */
  data?: unknown;
  /** What happened, in one line. */
  message: string;
}

/** One running language server. */
export interface LspServerStatus {
  /** The server id (`pyright`, `rust-analyzer`...). */
  id: string;
  /** Its process id. */
  pid: number;
  /** The session that started it. */
  session_id: string;
  /** `ready`. */
  state: string;
  /** It executes project code, so it starts only in trusted workspaces. */
  trusted_only: boolean;
  /** Seconds since it started. */
  uptime_secs: number;
  /** Its workspace root. */
  workspace: string;
}

/** `lsp.status` result. */
export interface LspStatus {
  /** Every running server of the profile, by id and workspace. */
  servers: Array<LspServerStatus>;
}

/** `lsp.status` params. */
export interface LspStatusParams {
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** One MCP server of a session. */
export interface McpServerState {
  /** Its configured name. */
  name: string;
  /** `ready`, `starting`, `failed` or `disabled`. */
  state: string;
  /** The tools it contributes. */
  tools: number;
}

/** `memory.summary` result. */
export interface MemorySummary {
  /** `Memory: +<added> ~<patched> -<removed> (review with /memory review)` when the agent changed memory in the session and `memory.review_mode` is not `off`; `null` otherwise. */
  line?: string | null;
}

/** One data point of a metric. */
export interface MetricPoint {
  /** Its bounded attributes (never content, paths or user ids). */
  attributes: Record<string, string>;
  /** A histogram's per-bucket counts (the bounds of `bounds`, then the overflow bucket). */
  buckets?: Array<number> | null;
  /** A histogram's sum of observations. */
  sum?: number | null;
  /** A counter's or gauge's value; a histogram's observation count. */
  value: number;
}

/** One metric of the catalog. */
export interface MetricView {
  /** What it counts. */
  description: string;
  /** `counter`, `histogram` or `gauge`. */
  kind: string;
  /** `prometheus.tokens`... */
  name: string;
  /** Its data points. */
  points: Array<MetricPoint>;
  /** The UCUM unit. */
  unit: string;
}

/** `system.metrics` result: the metrics catalog now. */
export interface MetricsSnapshot {
  /** Upper bucket bounds of every histogram, in milliseconds. */
  bounds: Array<number>;
  /** Every catalog metric. */
  metrics: Array<MetricView>;
}

/** One preset as editors show it. */
export interface MoaPresetInfo {
  /** The model that acts with the tools and is billed. */
  aggregator: MoaSlotInfo;
  /** Temperature sent to the aggregator. */
  aggregator_temperature?: number | null;
  /** `Aggregator is on <provider>; ...` when it bills another provider than `model.provider`. */
  billing_notice?: string | null;
  /** The built-in `default`, not redefined in `config.toml`. */
  builtin: boolean;
  /** It is `moa.default_preset` (cannot be deleted). */
  default: boolean;
  /** `false`: the aggregator runs alone. */
  enabled: boolean;
  /** When the advisors run (`user_turn`, `per_iteration`, `every_n:N`). */
  fanout: string;
  /** The preset name (`/model <name>`, `moa/<name>`). */
  name: string;
  /** Temperature sent to the references. */
  reference_temperature?: number | null;
  /** The advisors. */
  references: Array<MoaSlotInfo>;
}

/** A preset to write. */
export interface MoaPresetInput {
  /** The aggregator (never `moa`). */
  aggregator: MoaSlotInfo;
  /** Temperature sent to the aggregator (0.0-2.0). */
  aggregator_temperature?: number | null;
  /** Whether the references run (default `true`). */
  enabled?: boolean | null;
  /** The cadence (default `user_turn`). */
  fanout?: string | null;
  /** Temperature sent to the references (0.0-2.0). */
  reference_temperature?: number | null;
  /** The advisors (at least one). */
  references: Array<MoaSlotInfo>;
}

/** `moa.presets.list` (and `put`, `delete`) result. */
export interface MoaPresetList {
  /** `moa.default_preset`. */
  default_preset: string;
  /** Every preset, sorted by name. */
  presets: Array<MoaPresetInfo>;
}

/** `moa.presets.delete` params. */
export interface MoaPresetsDeleteParams {
  /** The preset name (never `moa.default_preset`). */
  name: string;
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** `moa.presets.list` params. */
export interface MoaPresetsListParams {
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** `moa.presets.put` params. */
export interface MoaPresetsPutParams {
  /** The preset name. */
  name: string;
  /** The preset. */
  preset: MoaPresetInput;
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** One preset slot. */
export interface MoaSlotInfo {
  /** Model id. */
  model: string;
  /** Provider id. */
  provider: string;
  /** `none`...`ultra`, when set. */
  reasoning_effort?: string | null;
}

/** One `model.options` entry. */
export interface ModelOption {
  /** The model's context window, when known. */
  context_window?: number | null;
  /** `<provider>/<model>`. */
  id: string;
  /** The most reasoning the model takes (`none` when it does not reason); a higher requested level is clamped to it. */
  max_reasoning?: string | null;
  /** The provider. */
  provider: string;
  /** Why switching the session to it is risky (the conversation does not fit its window, it cannot call tools...); clients confirm. */
  risky?: string | null;
}

/** `model.options` result. */
export interface ModelOptions {
  /** The session's model (`<provider>/<model>`). */
  current?: string | null;
  /** The models a `/model` switch may pick, current first. */
  models: Array<ModelOption>;
}

/** `model.options` params: a session's options, or a profile's before any session exists (the composer's picker of a new chat). */
export interface ModelOptionsParams {
  /** The profile, when no session is named. */
  profile?: string | null;
  /** The session (it must be attached). */
  session_id?: string | null;
}

/** A server-to-client request still waiting for an answer. */
export interface OpenRequest {
  /** `srq-<12 hex>`. */
  id: string;
  /** The request name (`approval.request`, ...). */
  method: string;
  /** Its params. */
  params: unknown;
}

/** `pairing.approve` params. */
export interface PairingApproveParams {
  /** The code (spaces, dashes and case are ignored). */
  code: string;
  /** The platform the code was issued on. */
  platform: string;
  /** The profile; required when the connection is granted several. */
  profile?: string | null;
}

/** `pairing.approve` result. */
export interface PairingApproved {
  /** The platform. */
  platform: string;
  /** The approved sender's platform id. */
  sender_id: string;
  /** The sender's display name, when known. */
  sender_name?: string | null;
}

/** The daemon's build target and its support tier. */
export interface PlatformSupport {
  /** The support matrix of this host, from runtime probes (spec: web-dashboard, Honest platform support matrix). */
  features?: Array<FeatureSupport>;
  /** The Rust target triple. */
  target: string;
  /** `tier1`, `tier2` or `unsupported`. */
  tier: string;
}

/** A call that only names the window's profile. */
export interface ProfileParams {
  /** The window's profile. */
  profile: string;
}

/** An image attached to a prompt: a file the daemon reads (checked like `chat --image`), or an image the session already holds (as `command.dispatch` returns them for `/retry`). */
export type PromptImage = {
  path: string;
} | {
  blob: string;
} | {
  data: string;
};

/** `queue.edit` params. */
export interface QueueEditParams {
  /** The queued input. */
  id: string;
  /** The session. */
  session_id: string;
  /** Its new text. */
  text: string;
}

/** One input waiting in a session's queue. */
export interface QueueEntry {
  /** The input's turn id. */
  id: string;
  /** A turn whose outcome was lost when the daemon stopped during it: it blocks the queue until it is deleted (Discard), and is never sent again. */
  lost?: boolean;
  /** The prompt text. */
  text: string;
}

/** `queue.delete` params. */
export interface QueueItemParams {
  /** The queued input (or the lost turn, to discard it). */
  id: string;
  /** The session. */
  session_id: string;
}

/** A session's queue. */
export interface QueueState {
  /** In run order; a lost turn comes first. */
  entries: Array<QueueEntry>;
  /** Paused: no queued input starts until `queue.resume`. */
  paused: boolean;
  /** The session. */
  session_id: string;
}

/** A subscription's plan and quota windows. */
export interface QuotaStatus {
  /** The plan label. */
  plan?: string | null;
  /** The provider reports quota data. */
  reported: boolean;
  /** One line for a status display (`quota: not reported by provider`). */
  text: string;
  /** The quota windows. */
  windows?: Array<QuotaWindow>;
}

/** One quota window. */
export interface QuotaWindow {
  /** `5h`, `weekly`... */
  name: string;
  /** When it resets (RFC 3339). */
  resets_at?: string | null;
  /** Used share, 0-100. */
  used_percent: number;
}

/** `request.cancel` payload: a server request timed out or was cancelled. */
export interface RequestCancel {
  /** `srq-<12 hex>`. */
  id: string;
  /** `timeout`, `cancelled`, or `no_responder` (no attached driver can answer: none was attached when it was asked, or the last one left). */
  reason: string;
}

/** `request.resolved` payload: a client answered. */
export interface RequestResolved {
  /** The client that answered (`<client kind>-<connection number>`). */
  by_client: string;
  /** `srq-<12 hex>`. */
  id: string;
}

/** What `session.resolve` looks up. */
export type ResolveKind = "resume" | "continue";

/** `session.resolve` params: a chat's `-r`/`-c` reference. */
export interface ResolveParams {
  /** The client's working directory (scopes `-c` and `latest`). */
  cwd?: string | null;
  /** Resume or continue. */
  kind: ResolveKind;
  /** The profile; required only when the ticket grants several. */
  profile?: string | null;
  /** The reference (empty for a bare `-c`). */
  reference: string;
  /** The client terminal's identity (prefers the session last used from it for a bare `-c`). */
  terminal?: string | null;
}

/** `session.resume` params. */
export interface ResumeParams {
  /** Become the session's interactive owner; fails with `-32004 session.in_use` naming the owner's pid while another connection is. */
  exclusive?: boolean;
  /** `ses_<ULID>`. */
  session_id: string;
  /** How the session runs from now on; a loaded, idle session is reloaded with it. Absent: as it is (or the profile's defaults). */
  setup?: SessionSetup | null;
}

/** `secret.request` result. */
export interface SecretAnswer {
  /** The value; `None` declines. */
  value?: string | null;
}

/** `secret.request` params. */
export interface SecretRequestParams {
  /** Milliseconds until the request settles unanswered (set by the daemon when it sends the request). */
  deadline_ms?: number | null;
  /** The secret's name. */
  name: string;
  /** Why it is needed. */
  reason?: string | null;
  /** The session. */
  session_id: string;
}

/** What the daemon serves. */
export interface ServerCapabilities {
  /** The events it may send this client. */
  events: Array<string>;
  /** The methods with a handler (the rest of the catalog answers `rpc.not_served`). */
  methods: Array<string>;
  /** The server-to-client requests it may send. */
  server_requests: Array<string>;
}

/** `session.create` params. */
export interface SessionCreateParams {
  /** The working directory of the session. */
  cwd?: string | null;
  /** Make this connection the session's interactive owner: another connection resuming it exclusively is refused while it is attached. */
  exclusive?: boolean;
  /** Retries with the same key within 300 s return the same session instead of creating another one. */
  idempotency_key?: string | null;
  /** The profile; required only when the ticket grants several. */
  profile?: string | null;
  /** How the session runs (a chat's flags). */
  setup?: SessionSetup;
  /** The client creating it; must be the connection's own `client` kind (`-32002 rpc.source_mismatch` otherwise). Absent: the connection's kind. */
  source?: ClientKind | null;
  /** A source tag recorded on the session. */
  source_tag?: string | null;
}

/** `session.list` result. */
export interface SessionList {
  /** The sessions. */
  sessions: Array<SessionSummary>;
}

/** `session.list` params. */
export interface SessionListParams {
  /** Also hidden sessions: canonical Bot Chats, archived and hidden ones. */
  include_hidden?: boolean;
  /** At most this many, newest activity first (default 50). */
  limit?: number | null;
  /** The profile; required only when the ticket grants several. */
  profile?: string | null;
}

/** A session named by id. */
export interface SessionRef {
  /** `ses_<ULID>`. */
  session_id: string;
}

/** How a daemon session runs: the session-shaping flags of `prometheus chat`, applied when the session loads (spec: cli, Daemon attachment with in-process fallback). */
export interface SessionSetup {
  /** That endpoint's API key (`--api-key-stdin`), held in memory only. */
  api_key?: string | null;
  /** An endpoint for this session only (`--base-url`). */
  base_url?: string | null;
  /** No rule files, identity file, memory snapshot or preloaded skills (`--ignore-rules`). */
  ignore_rules?: boolean;
  /** Schema defaults instead of the profile's `config.toml` (`--ignore-user-config`). */
  ignore_user_config?: boolean;
  /** A mock provider script on the daemon's machine (`--mock-script`). */
  mock_script?: string | null;
  /** The model (`[<provider>/]<model>` or an alias) instead of the profile's (`-m`). */
  model?: string | null;
  /** No `AGENTS.md`-style context files (`--no-context-files`). */
  no_context_files?: boolean;
  /** The system prompt names the session id (`--pass-session-id`). */
  pass_session_id?: boolean;
  /** The provider instead of the profile's (`--provider`). */
  provider?: string | null;
  /** `key=value` configuration overrides for this session only (`--set`, `--approvals`, `--max-iterations`, `--toolsets`). */
  set?: Array<string>;
  /** Skills preloaded into the session (`--skills`). */
  skills?: Array<string>;
  /** Approval-gated calls run without asking (`--yolo`). */
  yolo?: boolean;
}

/** One listed session. */
export interface SessionSummary {
  /** Whether a turn is running in the daemon. */
  live: boolean;
  /** `ses_<ULID>`. */
  session_id: string;
  /** When it started (RFC 3339). */
  started_at: string;
  /** Its title, when it has one. */
  title?: string | null;
}

/** How a steer is delivered. */
export type SteerKind = "note" | "redirect" | "wrap_up";

/** `session.steer` params. */
export interface SteerParams {
  /** How it is delivered (default `note`). */
  kind?: SteerKind;
  /** The session. */
  session_id: string;
  /** The steer; empty or whitespace-only text is refused (`empty steer`). */
  text: string;
}

/** `subagent.list` result. */
export interface SubagentList {
  /** The session's children, oldest first. */
  subagents: Array<SubagentSummary>;
}

/** One delegated child of a session. */
export interface SubagentSummary {
  /** Its child session. */
  session_id: string;
  /** `running`, `completed`, `failed` or `stopped`. */
  status: string;
  /** The child's id. */
  subagent_id: string;
  /** Its task's title. */
  title?: string | null;
}

/** One subcommand of a `commands.catalog` entry. */
export interface SubcommandEntry {
  /** Whether it runs from the asking client's surface. */
  disposition: Disposition;
  /** The first word after the command. */
  name: string;
  /** Why it does not run there. */
  reason?: string | null;
}

/** `prompt.submit` params. */
export interface SubmitParams {
  /** Never accepted: the gateway stamps every input's author from the connection, so a client-supplied one is refused with `-32002 rpc.forged_author`. */
  author?: unknown;
  /** Other files received with it (absolute paths on the daemon's machine): kept in the session's blob store and, with `attachments.storage = "workspace"`, copied into the workspace. */
  files?: Array<string>;
  /** Images that go with it. */
  images?: Array<PromptImage>;
  /** The client's id of this input: a resend with the same id returns the original admission instead of running a second turn. */
  input_id?: string | null;
  /** Wall-clock budget of this turn in seconds (`--run-budget`). */
  run_budget_secs?: number | null;
  /** The session. */
  session_id: string;
  /** The prompt. */
  text: string;
}

/** `prompt.submit` result: the input was admitted; its turn's output arrives as events. */
export interface Submitted {
  /** How the input was admitted. */
  admission?: Admission;
  /** What a `handled` input did, to show in place of a turn. */
  notice?: string | null;
  /** The admission id: the turn that runs the input (for a steered input, the running turn it joined). */
  turn_id: string;
}

/** A subscription's status in `session.context_usage`. */
export interface SubscriptionStatus {
  /** Its plan and quota windows. */
  quota: QuotaStatus;
}

/** `sudo.request` result. */
export interface SudoAnswer {
  /** The password; `None` declines. */
  password?: string | null;
}

/** `sudo.request` params. */
export interface SudoRequestParams {
  /** The command that needs the password. */
  command: string;
  /** Milliseconds until the request settles unanswered (set by the daemon when it sends the request). */
  deadline_ms?: number | null;
  /** The session. */
  session_id: string;
}

/** `system.health` result. */
export interface SystemHealth {
  /** The profile's components and the daemon-wide ones. */
  components: Array<ComponentHealth>;
}

/** `system.metrics` and `system.health` params. */
export interface SystemParams {
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** `session.transcript` result. */
export interface Transcript {
  /** The active messages, oldest first. */
  messages: Array<TranscriptMessage>;
}

/** One message of a transcript. */
export interface TranscriptMessage {
  /** `msg_<ULID>`. */
  id: string;
  /** `user`, `assistant`, `tool` or `system`. */
  role: string;
  /** Its text, when it has any. */
  text?: string | null;
}

/** Why a turn ended in error. */
export interface TurnError {
  /** The stable dotted error code. */
  code: string;
  /** The command that fixes it. */
  hint?: string | null;
  /** The error taxonomy kind (it fixes a CLI's exit status). */
  kind: string;
  /** One line. */
  message: string;
}

/** `turn.finished` payload. */
export interface TurnFinished {
  /** The error that ended the turn. */
  error?: TurnError | null;
  /** The turn's exit reason (`completed`, `interrupted_by_user`, ...). */
  exit_reason: string;
  /** The final answer, when there is one. */
  final_response?: string | null;
  /** The turn's `--usage-file` report (session, lineage root, route, billing source, tokens, cost and cost status of its model calls). */
  report?: unknown;
  /** The turn. */
  turn_id: string;
  /** The turn's token and cost totals, as the turn loop reports them. */
  usage?: unknown;
}

/** The machine's display preferences. */
export interface UiPrefs {
  /** Every stored preference. */
  prefs: Record<string, unknown>;
  /** Grows with every change. */
  revision: number;
}

/** `ui.prefs.get` params. */
export interface UiPrefsGetParams {
  /** Also send this connection `ui.prefs.changed` after every change. */
  watch?: boolean;
}

/** `ui.prefs.patch` params. */
export interface UiPrefsPatchParams {
  /** A client meeting the store for the first time: set only the keys the store lacks, never overwrite. */
  only_missing?: boolean;
  /** Keys to set; `null` removes one. */
  set: Record<string, unknown>;
}

/** Usage totals of a session or a lineage. */
export interface UsageTotalsView {
  /** Cache-read tokens. */
  cache_read_tokens: number;
  /** Cache-write tokens. */
  cache_write_tokens: number;
  /** Calls. */
  calls: number;
  /** Sum of the priced calls. */
  cost_usd: number;
  /** Uncached input tokens. */
  input_tokens: number;
  /** Output tokens. */
  output_tokens: number;
  /** Reasoning tokens. */
  reasoning_tokens: number;
  /** Calls without a price. */
  unpriced_calls: number;
}

/** `usage.updated` payload: after every model call, the same numbers for every client attached to the session. */
export interface UsageUpdated {
  /** Every configured budget. */
  budgets: Array<BudgetStanding>;
  /** The call's ledger row, without `request_hash`. */
  call: unknown;
  /** Its lineage root. */
  root_session_id: string;
  /** The lineage's totals. */
  root_totals: UsageTotalsView;
  /** The session that made the call. */
  session_id: string;
  /** The session's totals. */
  session_totals: UsageTotalsView;
}

/** `vault.add` params: exactly one of `login`, `card`, `address`. */
export interface VaultAddParams {
  /** An address. */
  address?: VaultAddressInput | null;
  /** A payment card. */
  card?: VaultCardInput | null;
  /** A name for the item. */
  label: string;
  /** A login. */
  login?: VaultLoginInput | null;
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** An address to add. */
export interface VaultAddressInput {
  /** City. */
  city: string;
  /** Country. */
  country: string;
  /** Street line. */
  line1: string;
  /** Second line. */
  line2?: string;
  /** Full name. */
  name: string;
  /** Postal code. */
  postal_code: string;
  /** State or region. */
  region?: string;
}

/** A payment card to add. */
export interface VaultCardInput {
  /** Security code. */
  cvc: string;
  /** `MM/YY`. */
  expiry: string;
  /** Cardholder name. */
  holder: string;
  /** Card number. */
  number: string;
}

/** One vault item's metadata (never a secret value; spec: security-boundaries, Vault management CLI). */
export interface VaultItem {
  /** `login_...`, `card_...` or `address_...`. */
  handle: string;
  /** A login's identifier. */
  identifier?: string | null;
  /** `login`, `card` or `address`. */
  kind: string;
  /** The label. */
  label: string;
  /** A login's origins. */
  origins: Array<string>;
  /** Whether a login has a TOTP seed. */
  totp: boolean;
}

/** `vault.list` result. */
export interface VaultItems {
  /** Every item's metadata. */
  items: Array<VaultItem>;
}

/** `vault.list` params. */
export interface VaultListParams {
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** A login to add. */
export interface VaultLoginInput {
  /** User name or email. */
  identifier: string;
  /** Origins it fills on (`https://host`). */
  origins: Array<string>;
  /** The password. */
  password: string;
  /** A base32 TOTP seed. */
  totp?: string | null;
}

/** `vault.remove` params. */
export interface VaultRemoveParams {
  /** The item's handle. */
  handle: string;
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** One password-manager source. */
export interface VaultSource {
  /** Whether this build can read it. */
  available: boolean;
  /** `vault.<name>.enabled`. */
  enabled: boolean;
  /** `onepassword` or `bitwarden`. */
  name: string;
}

/** `vault.sources` result. */
export interface VaultSourceList {
  /** The sources after the change. */
  sources: Array<VaultSource>;
}

/** `vault.sources` params: reads, or enables or disables one source. */
export interface VaultSourcesParams {
  /** `onepassword` or `bitwarden` to disable. */
  disable?: string | null;
  /** `onepassword` or `bitwarden` to enable. */
  enable?: string | null;
  /** The profile (the connection's only one when omitted). */
  profile?: string | null;
}

/** A `<major>.<minor>` protocol version on the wire. */
export interface Version {
  /** Incompatible revisions. */
  major: number;
  /** Backward-compatible additions. */
  minor: number;
}

/** One archive. */
export interface WorktreeArchiveInfo {
  /** Seconds since it was archived. */
  age_secs: number;
  /** The branch it was on. */
  branch?: string | null;
  /** The archived tree's name (`worktrees restore <name>`). */
  name: string;
  /** Why it was archived (`sweep`, `exit`, `subagent`). */
  reason: string;
  /** `refs/prometheus/archive/<name>`. */
  reference: string;
}

/** `worktree.new` result. */
export interface WorktreeCreated {
  /** The ref it was branched from. */
  base: string;
  /** What the user should know (a failed fetch). */
  notices: Array<string>;
  /** The new tree, now the session's working directory. */
  tree: WorktreeInfo;
}

/** One sweep decision. */
export interface WorktreeDecision {
  /** `remove`, `archive` or `keep`. */
  action: string;
  /** The tree. */
  name: string;
  /** What happened (the plan on a dry run). */
  outcome: string;
  /** Its directory. */
  path: string;
  /** The reason a tree is kept (`live-process`, `keep-large-untracked`...). */
  reason?: string | null;
}

/** One Prometheus tree. */
export interface WorktreeInfo {
  /** Seconds since it was created. */
  age_secs: number;
  /** The branch it is on (`null`: detached). */
  branch?: string | null;
  /** `generated`, `named`, `subagent` or `kanban`. */
  kind: string;
  /** The last sweep decision and its reason. */
  last_decision?: string | null;
  /** Its name (directory under `.worktrees/`). */
  name: string;
  /** Its directory. */
  path: string;
}

/** `worktree.list` result. */
export interface WorktreeListing {
  /** Its archives. */
  archives: Array<WorktreeArchiveInfo>;
  /** The repository's main checkout. */
  repo: string;
  /** Its Prometheus trees. */
  trees: Array<WorktreeInfo>;
}

/** `worktree.new` params. */
export interface WorktreeNewParams {
  /** The tree's name (`[A-Za-z0-9._-]{1,64}`); omitted: `prom-<8 hex>`. */
  name?: string | null;
  /** The session to move into the new tree. */
  session_id: string;
}

/** `worktree.prune` params. */
export interface WorktreePruneParams {
  /** Report the decisions; change nothing. */
  dry_run?: boolean;
  /** The session (its working directory's repository). */
  session_id: string;
}

/** `worktree.prune` result. */
export interface WorktreePruned {
  /** One decision per tree. */
  decisions: Array<WorktreeDecision>;
  /** Nothing was changed. */
  dry_run: boolean;
  /** The repository's main checkout. */
  repo: string;
}

/** Params naming the session (`worktree.status`, `worktree.list`). */
export interface WorktreeSessionParams {
  /** The session (its working directory's repository). */
  session_id: string;
}

/** `worktree.status` result. */
export interface WorktreeStatus {
  /** The session's working directory. */
  cwd: string;
  /** The Prometheus tree it is in (`null`: none). */
  tree?: WorktreeInfo | null;
}

export interface Methods {
  "approval.respond": { params: ApprovalRespondParams; result: Ack };
  "artifacts.list": { params: ArtifactsListParams; result: ArtifactList };
  "autoprune.offer": { params: AutopruneOfferParams; result: AutopruneOffer };
  "bot.chat.ensure": { params: ProfileParams; result: BotChat };
  "command.dispatch": { params: DispatchParams; result: Dispatched };
  "commands.catalog": { params: SessionRef; result: CommandCatalog };
  "complete.path": { params: CompleteParams; result: Completions };
  "complete.slash": { params: CompleteParams; result: Completions };
  "config.get": { params: ConfigGetParams; result: ConfigValue };
  "config.patch": { params: ConfigPatchParams; result: ConfigPatched };
  "config.reload": { params: ProfileParams; result: ConfigReloaded };
  "cron.blueprints.fill": { params: BlueprintFillParams; result: BlueprintFilled };
  "cron.blueprints.list": { params: BlueprintsListParams; result: BlueprintList };
  "egress.set": { params: EgressSetParams; result: EgressState };
  "files.rollback": { params: FilesRollbackParams; result: FilesRolledBack };
  "files.undo": { params: FilesUndoParams; result: FilesUndone };
  "fs.list": { params: FsListParams; result: FsListing };
  "fs.read": { params: FsReadParams; result: FsFile };
  "journey.list": { params: JourneyListParams; result: JourneyList };
  "local.add": { params: LocalParams; result: LocalReply };
  "local.catalog": { params: LocalParams; result: LocalReply };
  "local.install": { params: LocalParams; result: LocalReply };
  "local.jobs": { params: LocalParams; result: LocalReply };
  "local.pause": { params: LocalParams; result: LocalReply };
  "local.pull": { params: LocalParams; result: LocalReply };
  "local.resume": { params: LocalParams; result: LocalReply };
  "local.rm": { params: LocalParams; result: LocalReply };
  "local.search": { params: LocalParams; result: LocalReply };
  "local.start": { params: LocalParams; result: LocalReply };
  "local.status": { params: LocalParams; result: LocalReply };
  "local.stop": { params: LocalParams; result: LocalReply };
  "local.update": { params: LocalParams; result: LocalReply };
  "local.use": { params: LocalParams; result: LocalReply };
  "lsp.status": { params: LspStatusParams; result: LspStatus };
  "memory.summary": { params: SessionRef; result: MemorySummary };
  "moa.presets.delete": { params: MoaPresetsDeleteParams; result: MoaPresetList };
  "moa.presets.list": { params: MoaPresetsListParams; result: MoaPresetList };
  "moa.presets.put": { params: MoaPresetsPutParams; result: MoaPresetList };
  "model.options": { params: ModelOptionsParams; result: ModelOptions };
  "pairing.approve": { params: PairingApproveParams; result: PairingApproved };
  "prompt.submit": { params: SubmitParams; result: Submitted };
  "queue.delete": { params: QueueItemParams; result: QueueState };
  "queue.edit": { params: QueueEditParams; result: QueueState };
  "queue.list": { params: SessionRef; result: QueueState };
  "queue.pause": { params: SessionRef; result: QueueState };
  "queue.resume": { params: SessionRef; result: QueueState };
  "rpc.hello": { params: HelloParams; result: HelloResult };
  "rpc.ping": { params: Empty; result: Ack };
  "session.attach": { params: AttachParams; result: Attached };
  "session.banner": { params: SessionRef; result: BannerInfo };
  "session.compact": { params: CompactParams; result: Compacted };
  "session.context_usage": { params: SessionRef; result: ContextUsage };
  "session.create": { params: SessionCreateParams; result: SessionRef };
  "session.detach": { params: SessionRef; result: Ack };
  "session.events.since": { params: EventsSinceParams; result: EventsSince };
  "session.fork": { params: ForkParams; result: SessionRef };
  "session.interrupt": { params: SessionRef; result: Ack };
  "session.list": { params: SessionListParams; result: SessionList };
  "session.resolve": { params: ResolveParams; result: SessionRef };
  "session.resume": { params: ResumeParams; result: Attached };
  "session.siblings": { params: SessionRef; result: Lineage };
  "session.steer": { params: SteerParams; result: Ack };
  "session.transcript": { params: SessionRef; result: Transcript };
  "session.tree": { params: SessionRef; result: Lineage };
  "subagent.list": { params: SessionRef; result: SubagentList };
  "system.health": { params: SystemParams; result: SystemHealth };
  "system.metrics": { params: SystemParams; result: MetricsSnapshot };
  "ui.prefs.get": { params: UiPrefsGetParams; result: UiPrefs };
  "ui.prefs.patch": { params: UiPrefsPatchParams; result: UiPrefs };
  "vault.add": { params: VaultAddParams; result: VaultItem };
  "vault.list": { params: VaultListParams; result: VaultItems };
  "vault.remove": { params: VaultRemoveParams; result: VaultItem };
  "vault.sources": { params: VaultSourcesParams; result: VaultSourceList };
  "worktree.list": { params: WorktreeSessionParams; result: WorktreeListing };
  "worktree.new": { params: WorktreeNewParams; result: WorktreeCreated };
  "worktree.prune": { params: WorktreePruneParams; result: WorktreePruned };
  "worktree.status": { params: WorktreeSessionParams; result: WorktreeStatus };
}

export interface ServerRequests {
  "approval.request": { params: ApprovalRequestParams; result: ApprovalAnswer };
  "clarify.request": { params: ClarifyRequestParams; result: ClarifyAnswers };
  "secret.request": { params: SecretRequestParams; result: SecretAnswer };
  "sudo.request": { params: SudoRequestParams; result: SudoAnswer };
}

export interface Events {
  "adapter.state": AdapterStatePayload;
  "automation.state": AutomationStatePayload;
  "health.changed": HealthChanged;
  "queue.changed": QueueState;
  "request.cancel": RequestCancel;
  "request.resolved": RequestResolved;
  "session.event": unknown;
  "turn.finished": TurnFinished;
  "ui.prefs.changed": UiPrefs;
  "usage.updated": UsageUpdated;
}

export const UNDECLARED = [
  "bot.create",
  "bot.meta.get",
  "bot.meta.update",
  "bot.relay.deliver",
  "bot.relay.drain",
  "bot.roster.list",
  "daemon.update",
  "evolve.cancel",
  "evolve.choose",
  "evolve.list",
  "evolve.proposals.adopt",
  "evolve.proposals.list",
  "evolve.proposals.reject",
  "evolve.start",
  "evolve.status",
  "experience.domains",
  "experience.forget",
  "experience.list",
  "experience.pin",
  "experience.restore",
  "experience.show",
  "grants.list",
  "grants.revoke",
  "groups.capabilities",
  "groups.log",
  "groups.replica_state",
  "groups.replicate",
  "harness.diff",
  "harness.log",
  "harness.rollback",
  "screen.handback",
  "screen.takeover",
  "subagent.subscribe",
  "tools.call",
  "tools.list",
  "voice.client_config",
  "wake.feed",
  "achievement.unlocked",
  "local_model.context_grown",
  "message.delivery",
  "message.reaction",
  "session.presence",
  "wake.detected",
] as const;
