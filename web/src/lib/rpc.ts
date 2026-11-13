// The client protocol over `/api/rpc` (the same JSON-RPC 2.0 protocol the
// CLI, TUI and desktop speak, types generated from the daemon's schema).
// The browser is a `dashboard` client that answers server requests
// (approvals, clarifications); the socket is authenticated by the session
// cookie and the CSRF token offered as a `csrf.<token>` subprotocol.

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

type Pending = { resolve: (v: unknown) => void; reject: (e: unknown) => void };

/** Seconds between `rpc.ping`s: the daemon closes a silent socket at 45 s. */
const PING_SECS = 15;

export class Rpc {
  private socket: WebSocket | null = null;
  private next = 1;
  private pending = new Map<number, Pending>();
  private ping: number | undefined;
  hello: HelloResult | null = null;
  onEvent: (event: Envelope) => void = () => {};
  onRequest: (request: ServerRequest) => void = () => {};
  onClose: (code: number) => void = () => {};

  async connect(): Promise<HelloResult> {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const socket = new WebSocket(`${scheme}://${location.host}${base}/api/rpc`, [
      `prometheus-rpc.v${PROTOCOL.major}`,
      `csrf.${csrfToken()}`,
    ]);
    this.socket = socket;
    socket.onmessage = (message) => this.receive(String(message.data));
    socket.onclose = (closed) => {
      window.clearInterval(this.ping);
      for (const waiter of this.pending.values()) waiter.reject(new RpcError({ code: closed.code, message: "connection closed" }));
      this.pending.clear();
      this.onClose(closed.code);
    };
    await new Promise<void>((resolve, reject) => {
      socket.onopen = () => resolve();
      socket.onerror = () => reject(new Error("cannot reach the daemon"));
    });
    this.hello = (await this.call("rpc.hello", {
      client: "dashboard",
      client_version: "web",
      protocol: PROTOCOL,
      capabilities: { server_requests: true, events: [] },
    })) as HelloResult;
    this.ping = window.setInterval(() => void this.call("rpc.ping", {}).catch(() => {}), PING_SECS * 1000);
    return this.hello;
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
      this.pending.set(id, { resolve, reject });
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
