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

/** `session.attach` / `session.resume` result. */
export interface Attached {
  /** The daemon epoch the sequence numbers belong to. */
  epoch: string;
  /** The newest event sequence number kept for it. */
  latest_seq: number;
  /** The session. */
  session_id: string;
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

/** `session.context_usage` result. */
export interface ContextUsage {
  /** The model's context window, when known. */
  context_window?: number | null;
  /** Estimated tokens of the active context. */
  tokens: number;
}

/** An approval answer. */
export type Decision = "once" | "session" | "always" | "deny";

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

/** `session.fork` params. */
export interface ForkParams {
  /** The last message the fork keeps (`msg_<ULID>`); default the session's last active message. */
  at_message_id?: string | null;
  /** The session to fork. */
  session_id: string;
  /** The fork's title; default `<parent title> #<n>`. */
  title?: string | null;
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

/** A server-to-client request still waiting for an answer. */
export interface OpenRequest {
  /** `srq-<12 hex>`. */
  id: string;
  /** The request name (`approval.request`, ...). */
  method: string;
  /** Its params. */
  params: unknown;
}

/** The daemon's build target and its support tier. */
export interface PlatformSupport {
  /** The Rust target triple. */
  target: string;
  /** `tier1`, `tier2` or `unsupported`. */
  tier: string;
}

/** `request.cancel` payload: a server request timed out or was cancelled. */
export interface RequestCancel {
  /** `srq-<12 hex>`. */
  id: string;
  /** `timeout` or `cancelled`. */
  reason: string;
}

/** `request.resolved` payload: another client answered. */
export interface RequestResolved {
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
}

/** `secret.request` result. */
export interface SecretAnswer {
  /** The value; `None` declines. */
  value?: string | null;
}

/** `secret.request` params. */
export interface SecretRequestParams {
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
  /** The model for this session (`[<provider>/]<model>` or an alias) instead of the profile's. */
  model?: string | null;
  /** The profile; required only when the ticket grants several. */
  profile?: string | null;
  /** The provider for this session instead of the profile's. */
  provider?: string | null;
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

/** `prompt.submit` params. */
export interface SubmitParams {
  /** The session. */
  session_id: string;
  /** The prompt. */
  text: string;
}

/** `prompt.submit` result: the turn started; its output arrives as events. */
export interface Submitted {
  /** The turn. */
  turn_id: string;
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
  /** The session. */
  session_id: string;
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
  /** The turn. */
  turn_id: string;
  /** The turn's token and cost totals, as the turn loop reports them. */
  usage?: unknown;
}

/** A `<major>.<minor>` protocol version on the wire. */
export interface Version {
  /** Incompatible revisions. */
  major: number;
  /** Backward-compatible additions. */
  minor: number;
}

export interface Methods {
  "approval.respond": { params: ApprovalRespondParams; result: Ack };
  "prompt.submit": { params: SubmitParams; result: Submitted };
  "rpc.hello": { params: HelloParams; result: HelloResult };
  "rpc.ping": { params: Empty; result: Ack };
  "session.attach": { params: SessionRef; result: Attached };
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
  "session.transcript": { params: SessionRef; result: Transcript };
}

export interface ServerRequests {
  "approval.request": { params: ApprovalRequestParams; result: ApprovalAnswer };
  "clarify.request": { params: ClarifyRequestParams; result: ClarifyAnswers };
  "secret.request": { params: SecretRequestParams; result: SecretAnswer };
  "sudo.request": { params: SudoRequestParams; result: SudoAnswer };
}

export interface Events {
  "request.cancel": RequestCancel;
  "request.resolved": RequestResolved;
  "session.event": unknown;
  "turn.finished": TurnFinished;
}

export const UNDECLARED = [
  "bot.chat.ensure",
  "bot.create",
  "bot.meta.get",
  "bot.meta.update",
  "bot.relay.deliver",
  "bot.relay.drain",
  "bot.roster.list",
  "command.dispatch",
  "commands.catalog",
  "complete.path",
  "complete.slash",
  "cron.blueprints.fill",
  "cron.blueprints.list",
  "daemon.update",
  "egress.set",
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
  "files.rollback",
  "files.undo",
  "grants.list",
  "grants.revoke",
  "groups.capabilities",
  "groups.log",
  "groups.replica_state",
  "groups.replicate",
  "harness.diff",
  "harness.log",
  "harness.rollback",
  "journey.list",
  "lsp.status",
  "moa.presets.delete",
  "moa.presets.list",
  "moa.presets.put",
  "model.options",
  "pairing.approve",
  "screen.handback",
  "screen.takeover",
  "subagent.list",
  "subagent.subscribe",
  "system.health",
  "system.metrics",
  "tools.call",
  "tools.list",
  "ui.prefs.get",
  "ui.prefs.patch",
  "vault.add",
  "vault.list",
  "vault.remove",
  "vault.sources",
  "voice.client_config",
  "wake.feed",
  "worktree.list",
  "worktree.new",
  "worktree.prune",
  "worktree.status",
  "achievement.unlocked",
  "adapter.state",
  "automation.state",
  "health.changed",
  "local_model.context_grown",
  "message.delivery",
  "message.reaction",
  "session.presence",
  "usage.updated",
  "wake.detected",
] as const;
