// Embed Prometheus in a Node program (spec: workspace-architecture,
// Embeddable client SDKs), over the daemon's versioned RPC protocol
// (`schema/rpc.json`, whose types are `schema/rpc.ts`):
//
//   const client = await Client.connect();
//   const session = await client.sessions.create({ toolsets: ["file"] });
//   for await (const event of session.prompt("list files")) console.log(event.type);
//   console.log(session.result?.usage);
//
// `connect` runs `prometheus daemon ensure` (attaching to the running
// daemon or starting one), checks the daemon's protocol, then gets a
// single-use ticket with `prometheus daemon ticket` (the ticketed loopback
// flow every client uses). A daemon whose protocol major is not in
// SUPPORTED_PROTOCOL_MAJORS is refused with ProtocolMismatch before any
// ticket or request; so is a -32008 answer to `rpc.hello`. Needs Node 22
// (its global WebSocket). Only erasable TypeScript is used, so Node runs
// this file as is.

import { execFile } from "node:child_process";
import { promisify } from "node:util";

/** The protocol majors this release speaks. */
export const SUPPORTED_PROTOCOL_MAJORS: readonly number[] = [1];
export const PROTOCOL_MINOR = 0;
export const VERSION = "0.1.0";
/** The JSON-RPC error code of a protocol mismatch at `rpc.hello`. */
export const PROTOCOL_MISMATCH_CODE = -32008;
/** The daemon drops a connection silent for 45 s. */
const HEARTBEAT_MS = 15_000;

type Json = Record<string, unknown>;

export class RpcError extends Error {
  readonly code: number;
  readonly data: unknown;
  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.code = code;
    this.data = data;
  }
}

export class ProtocolMismatch extends Error {}

/** One event of a turn: a trajectory event (`tool.call`, `tool.result`, `assistant.message`...). */
export interface Event {
  type: string;
  data: Json;
}

/** How a turn ended (`turn.finished`). */
export interface Result {
  exitReason: string;
  finalResponse: string | null;
  usage: Json;
  error: Json | null;
}

export interface ConnectOptions {
  home?: string;
  profile?: string;
  /** The `prometheus` binary (default: on PATH). */
  binary?: string;
  /** Answers an approval request with `once`, `session`, `always` or `deny`. */
  onApproval?: (params: Json) => string | Promise<string>;
  /** Answers clarifying questions, one answer per question. */
  onClarify?: (params: Json) => unknown[] | Promise<unknown[]>;
}

export interface CreateOptions {
  toolsets?: string[];
  model?: string;
  /** Sent as a last system block with every model call of this session only, never stored. */
  systemPromptOverlay?: string;
  /** Keep the transcript out of the session store and trajectories. */
  ephemeral?: boolean;
}

/** The major of `<major>.<minor>`, refused when unsupported. */
export function checkProtocol(version: string): number {
  const major = Number(version.split(".")[0]);
  if (!SUPPORTED_PROTOCOL_MAJORS.includes(major)) {
    throw new ProtocolMismatch(
      `the daemon speaks protocol ${version}; this SDK supports major ${SUPPORTED_PROTOCOL_MAJORS.join(", ")}`,
    );
  }
  return major;
}

/** `prometheus --json <command>`'s answer. */
async function cli(options: ConnectOptions, ...command: string[]): Promise<Json> {
  const args = [
    ...(options.home ? ["--home", options.home] : []),
    ...(options.profile ? ["--profile", options.profile] : []),
    "--json",
    ...command,
  ];
  const { stdout } = await promisify(execFile)(options.binary ?? "prometheus", args);
  return JSON.parse(stdout) as Json;
}

type Item = { name: string; data: Json } | null;

/** A queue of a session's events that a prompt iterates. */
class Channel {
  private items: Item[] = [];
  private waiting: ((item: Item) => void) | null = null;

  push(item: Item): void {
    const wake = this.waiting;
    if (wake) {
      this.waiting = null;
      wake(item);
    } else {
      this.items.push(item);
    }
  }

  next(): Promise<Item> {
    if (this.items.length > 0) return Promise.resolve(this.items.shift() ?? null);
    return new Promise((resolve) => {
      this.waiting = resolve;
    });
  }
}

export class Client {
  hello: Json = {};
  readonly sessions: Sessions;
  readonly profile: string;
  private readonly ws: WebSocket;
  private readonly options: ConnectOptions;
  private next = 0;
  private readonly pending = new Map<number, (frame: Json) => void>();
  private readonly channels = new Map<string, Channel>();
  private heartbeat: ReturnType<typeof setInterval> | undefined;

  private constructor(ws: WebSocket, profile: string, options: ConnectOptions) {
    this.ws = ws;
    this.profile = profile;
    this.options = options;
    this.sessions = new Sessions(this);
    ws.addEventListener("message", (message) => this.receive(JSON.parse(String(message.data)) as Json));
    ws.addEventListener("close", () => this.closed());
  }

  /** Attaches to the daemon of `home` (starting it when needed) for `profile` (default `default`). */
  static async connect(options: ConnectOptions = {}): Promise<Client> {
    const endpoint = await cli(options, "daemon", "ensure");
    const major = checkProtocol(String(endpoint.rpc_protocol));
    const issued = await cli(options, "daemon", "ticket");
    const ws = new WebSocket(String(endpoint.rpc_url), [`prometheus-rpc.v${major}`, `prometheus-ticket.${String(issued.ticket)}`]);
    await new Promise<void>((resolve, reject) => {
      ws.addEventListener("open", () => resolve(), { once: true });
      ws.addEventListener("error", () => reject(new Error("cannot reach the daemon")), { once: true });
    });
    const client = new Client(ws, options.profile ?? "default", options);
    await client.sayHello(major);
    client.heartbeat = setInterval(() => {
      client.call("rpc.ping").catch(() => undefined);
    }, HEARTBEAT_MS);
    return client;
  }

  private async sayHello(major: number): Promise<void> {
    // Without callbacks the daemon resolves its requests unanswered.
    const answers = Boolean(this.options.onApproval ?? this.options.onClarify);
    try {
      this.hello = await this.call<Json>("rpc.hello", {
        client: "cli",
        client_version: VERSION,
        protocol: { major, minor: PROTOCOL_MINOR },
        capabilities: { server_requests: answers, events: [] },
      });
    } catch (err) {
      if (err instanceof RpcError && err.code === PROTOCOL_MISMATCH_CODE) {
        this.close();
        throw new ProtocolMismatch(err.message);
      }
      throw err;
    }
    const protocol = (this.hello.protocol ?? {}) as { major?: number; minor?: number };
    checkProtocol(`${protocol.major}.${protocol.minor ?? 0}`);
  }

  /** One request: its result, or an RpcError. */
  call<T = unknown>(method: string, params: Json = {}): Promise<T> {
    this.next += 1;
    const id = this.next;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, (frame) => {
        const error = frame.error as { code: number; message: string; data?: unknown } | undefined;
        if (error) reject(new RpcError(error.code, error.message, error.data));
        else resolve(frame.result as T);
      });
      this.ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  }

  /** The event queue of `session`. */
  channel(session: string): Channel {
    let channel = this.channels.get(session);
    if (!channel) {
      channel = new Channel();
      this.channels.set(session, channel);
    }
    return channel;
  }

  private receive(frame: Json): void {
    if (typeof frame.method === "string" && frame.id !== undefined) {
      void this.answer(frame);
    } else if (typeof frame.method === "string") {
      const envelope = (frame.params ?? {}) as { event?: string; session_id?: string; data?: Json };
      this.channel(envelope.session_id ?? "").push({ name: envelope.event ?? "", data: envelope.data ?? {} });
    } else if (typeof frame.id === "number") {
      this.pending.get(frame.id)?.(frame);
      this.pending.delete(frame.id);
    }
  }

  /** A server request (an approval or a clarifying question). */
  private async answer(request: Json): Promise<void> {
    const params = (request.params ?? {}) as Json;
    let reply: Json;
    if (request.method === "approval.request" && this.options.onApproval) {
      reply = { result: { decision: await this.options.onApproval(params) } };
    } else if (request.method === "clarify.request" && this.options.onClarify) {
      reply = { result: { answers: await this.options.onClarify(params) } };
    } else {
      reply = { error: { code: -32601, message: `unknown method: ${String(request.method)}` } };
    }
    this.ws.send(JSON.stringify({ jsonrpc: "2.0", id: request.id, ...reply }));
  }

  private closed(): void {
    clearInterval(this.heartbeat);
    for (const settle of this.pending.values()) settle({ error: { code: -32000, message: "connection closed" } });
    this.pending.clear();
    for (const channel of this.channels.values()) channel.push(null);
  }

  close(): void {
    clearInterval(this.heartbeat);
    this.ws.close();
  }
}

export class Sessions {
  private readonly client: Client;

  constructor(client: Client) {
    this.client = client;
  }

  /** A new session of the client's profile; the creating connection receives its events. */
  async create(options: CreateOptions = {}): Promise<Session> {
    const setup: Json = { ephemeral: options.ephemeral ?? false };
    if (options.toolsets) setup.set = [`tools.enabled_toolsets=${JSON.stringify(options.toolsets)}`];
    if (options.model) setup.model = options.model;
    if (options.systemPromptOverlay) setup.system_prompt_overlay = options.systemPromptOverlay;
    const created = await this.client.call<{ session_id: string }>("session.create", { setup });
    return new Session(this.client, created.session_id);
  }
}

export class Session {
  result: Result | null = null;
  readonly id: string;
  private readonly client: Client;

  constructor(client: Client, id: string) {
    this.client = client;
    this.id = id;
  }

  /** Runs one turn: yields its events, then sets `result`. */
  async *prompt(text: string, attachments: string[] = []): AsyncGenerator<Event> {
    const channel = this.client.channel(this.id);
    const params: Json = { session_id: this.id, text };
    if (attachments.length > 0) params.files = attachments;
    const admitted = await this.client.call<{ turn_id?: string }>("prompt.submit", params);
    for (;;) {
      const item = await channel.next();
      if (item === null) throw new Error("the daemon closed the connection");
      if (item.name === "turn.finished" && (!admitted.turn_id || item.data.turn_id === admitted.turn_id)) {
        this.result = {
          exitReason: String(item.data.exit_reason ?? ""),
          finalResponse: (item.data.final_response as string | null) ?? null,
          usage: (item.data.usage as Json | null) ?? {},
          error: (item.data.error as Json | null) ?? null,
        };
        return;
      }
      if (item.name === "session.event") yield { type: String(item.data.type ?? ""), data: item.data };
    }
  }

  /** Stops the running turn at its next safe point. */
  async interrupt(): Promise<void> {
    await this.client.call("session.interrupt", { session_id: this.id });
  }

  /** The session's messages so far (multi-turn use). */
  async history(): Promise<Json[]> {
    const transcript = await this.client.call<{ messages: Json[] }>("session.transcript", { session_id: this.id });
    return transcript.messages;
  }
}
