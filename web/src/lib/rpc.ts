// The client protocol over WebSocket (the same JSON-RPC 2.0 protocol the
// CLI, TUI and desktop speak, types generated from the daemon's schema),
// shared by the dashboard and the desktop app. The dashboard is a
// `dashboard` client on `/api/rpc`, authenticated by the session cookie and
// the CSRF token offered as a `csrf.<token>` subprotocol; the desktop dials
// the daemon itself with a one-use ticket subprotocol. Both answer server
// requests (approvals, clarifications).

import { PROTOCOL } from "../../../schema/rpc";
import type { Envelope, HelloResult, Methods } from "../../../schema/rpc";
import { base, csrfToken } from "./api";

export interface RpcFailure {
  code: number;
  message: string;
  data?: { code?: string; hint?: string };
}

export class RpcError extends Error {
  constructor(readonly failure: RpcFailure) {
    super(failure.message);
  }
  get code(): number {
    return this.failure.code;
  }
}

/** A server-to-client request (`approval.request`, `clarify.request`...). */
export interface ServerRequest {
  id: string;
  method: string;
  params: Record<string, unknown>;
}

/** Where and how to open the socket. */
export interface Dial {
  url: string;
  protocols: string[];
}

/** How a client identifies itself and how long it waits. */
export interface RpcOptions {
  client: "dashboard" | "desktop";
  clientVersion: string;
  /** A request unanswered this long fails (0: never). */
  requestTimeoutMs: number;
  /** A socket not open this long fails (0: never). */
  connectTimeoutMs: number;
  /** Ping interval: the daemon closes a socket silent for 45 s. */
  heartbeatMs: number;
  /** A socket that received nothing this long is dead (0: never). */
  deadAfterMs: number;
}

const DASHBOARD: RpcOptions = {
  client: "dashboard",
  clientVersion: "web",
  requestTimeoutMs: 0,
  connectTimeoutMs: 0,
  heartbeatMs: 15_000,
  deadAfterMs: 0,
};

/** The dashboard's `/api/rpc` on its own origin. */
export function dashboardDial(): Dial {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return {
    url: `${scheme}://${location.host}${base}/api/rpc`,
    protocols: [`prometheus-rpc.v${PROTOCOL.major}`, `csrf.${csrfToken()}`],
  };
}

type Pending = { resolve: (v: unknown) => void; reject: (e: unknown) => void; timer?: number };

/** Closed by this side because nothing arrived within `deadAfterMs`. */
export const DEAD_CLOSE = 4000;

export class Rpc {
  private socket: WebSocket | null = null;
  private next = 1;
  private pending = new Map<number, Pending>();
  private ping: number | undefined;
  private lastFrame = 0;
  hello: HelloResult | null = null;
  onEvent: (event: Envelope) => void = () => {};
  onRequest: (request: ServerRequest) => void = () => {};
  onClose: (code: number) => void = () => {};

  constructor(private readonly options: RpcOptions = DASHBOARD) {}

  /** Opens the socket and sends `rpc.hello` as its first frame. */
  async connect(dial: Dial = dashboardDial()): Promise<HelloResult> {
    const socket = new WebSocket(dial.url, dial.protocols);
    this.socket = socket;
    socket.onmessage = (message) => this.receive(String(message.data));
    socket.onclose = (closed) => this.closed(closed.code);
    await this.opened(socket);
    this.lastFrame = Date.now();
    this.hello = (await this.call("rpc.hello", {
      client: this.options.client,
      client_version: this.options.clientVersion,
      protocol: PROTOCOL,
      capabilities: { server_requests: true, events: [] },
    })) as HelloResult;
    this.ping = window.setInterval(() => this.beat(), this.options.heartbeatMs);
    return this.hello;
  }

  private opened(socket: WebSocket): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const timeout = this.options.connectTimeoutMs;
      const timer =
        timeout > 0
          ? window.setTimeout(() => {
              socket.close();
              reject(new Error("the daemon did not answer in time"));
            }, timeout)
          : undefined;
      socket.onopen = () => {
        window.clearTimeout(timer);
        resolve();
      };
      socket.onerror = () => {
        window.clearTimeout(timer);
        reject(new Error("cannot reach the daemon"));
      };
    });
  }

  private beat(): void {
    const dead = this.options.deadAfterMs;
    if (dead > 0 && Date.now() - this.lastFrame > dead) {
      this.socket?.close(DEAD_CLOSE, "no frame received");
      return;
    }
    void this.call("rpc.ping", {}).catch(() => {});
  }

  private closed(code: number): void {
    window.clearInterval(this.ping);
    for (const waiter of this.pending.values()) {
      window.clearTimeout(waiter.timer);
      waiter.reject(new RpcError({ code, message: "connection closed" }));
    }
    this.pending.clear();
    this.onClose(code);
  }

  /** A typed call of a method of the catalog. */
  request<M extends keyof Methods>(method: M, params: Methods[M]["params"]): Promise<Methods[M]["result"]> {
    return this.call(method, params) as Promise<Methods[M]["result"]>;
  }

  /** Any call (methods the generated schema does not declare yet). */
  call(method: string, params: unknown): Promise<unknown> {
    const id = this.next++;
    const frame = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolve, reject) => {
      const waiter: Pending = { resolve, reject };
      const timeout = this.options.requestTimeoutMs;
      if (timeout > 0) {
        waiter.timer = window.setTimeout(() => {
          this.pending.delete(id);
          reject(new RpcError({ code: -32000, message: `${method} timed out` }));
        }, timeout);
      }
      this.pending.set(id, waiter);
      this.socket?.send(JSON.stringify(frame));
    });
  }

  /** Answers a server request with its result. */
  answer(id: string, result: unknown): void {
    this.socket?.send(JSON.stringify({ jsonrpc: "2.0", id, result }));
  }

  close(): void {
    this.socket?.close(1000);
  }

  private receive(text: string): void {
    this.lastFrame = Date.now();
    const frame = JSON.parse(text) as Record<string, unknown>;
    if (typeof frame.method === "string" && frame.id !== undefined) {
      this.onRequest({ id: String(frame.id), method: frame.method, params: (frame.params ?? {}) as Record<string, unknown> });
      return;
    }
    if (typeof frame.method === "string") {
      this.onEvent(frame.params as Envelope);
      return;
    }
    const waiter = this.pending.get(Number(frame.id));
    if (!waiter) return;
    this.pending.delete(Number(frame.id));
    window.clearTimeout(waiter.timer);
    if (frame.error) waiter.reject(new RpcError(frame.error as RpcFailure));
    else waiter.resolve(frame.result);
  }
}

/** The page's one protocol connection, opened on first use. */
let shared: Promise<Rpc> | null = null;

export function rpc(): Promise<Rpc> {
  if (!shared) {
    const client = new Rpc();
    shared = client.connect().then(() => client);
    shared.catch(() => {
      shared = null;
    });
    client.onClose = () => {
      shared = null;
    };
  }
  return shared;
}
