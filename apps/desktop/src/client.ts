// The desktop's protocol client (spec: desktop-app, One-use dial tickets;
// Shared client defaults; Reconnect backoff with full jitter; Profile
// switching is explicit). Every dial asks the shell for a fresh one-use
// ticket for the window's profile; every connection, reconnects included,
// starts with `rpc.hello` as a `desktop` client that answers server
// requests; a lost connection is redialled with full-jitter backoff and
// every session open in the window is re-attached with its last seen
// sequence number.

import type { Envelope } from "../../../schema/rpc";
import { Rpc, RpcError, type RpcOptions, type ServerRequest } from "../../../web/src/lib/rpc";
import { Backoff } from "./backoff";
import { type Dial, shell } from "./shell";

export const DESKTOP: RpcOptions = {
  client: "desktop",
  clientVersion: "desktop",
  requestTimeoutMs: 120_000,
  connectTimeoutMs: 15_000,
  heartbeatMs: 15_000,
  deadAfterMs: 45_000,
};

/** Where a session's event stream was left. */
interface Watermark {
  seq: number | null;
  epoch: string | null;
}

/** `-32006`: the daemon is unavailable (retiring for a restart). */
const RETIRING = -32006;

export type Connection = "connecting" | "connected" | "reconnecting";

export class DesktopClient {
  readonly backoff = new Backoff();
  state: Connection = "connecting";
  switchingProfile = false;
  private rpc: Rpc | null = null;
  private retry: number | undefined;
  private leaving = false;
  private readonly open = new Map<string, Watermark>();
  onEvent: (envelope: Envelope) => void = () => {};
  onRequest: (request: ServerRequest) => void = () => {};
  onState: (state: Connection) => void = () => {};
  /** A re-attached session whose missed events were evicted. */
  onTruncated: (sessionId: string) => void = () => {};

  constructor(public profile: string) {}

  get reconnecting(): boolean {
    return this.state !== "connected";
  }

  /** Dials; a failed first dial keeps retrying with backoff. */
  start(): void {
    this.connect().catch((err: unknown) => {
      console.warn("gateway dial failed:", err);
      this.backoff.failed();
      this.setState("reconnecting");
      this.schedule();
    });
  }

  private waiting: (() => void)[] = [];

  private setState(state: Connection): void {
    this.state = state;
    this.onState(state);
    if (state === "connected") for (const resume of this.waiting.splice(0)) resume();
  }

  /** Resolves once connected (at once when already). */
  ready(): Promise<void> {
    if (this.state === "connected") return Promise.resolve();
    return new Promise((resolve) => this.waiting.push(resolve));
  }

  private async connect(): Promise<void> {
    const dial = await shell<Dial>("prepare_dial", { profile: this.profile });
    const rpc = new Rpc(DESKTOP);
    rpc.onEvent = (envelope) => this.received(envelope);
    rpc.onRequest = (request) => this.onRequest(request);
    try {
      await rpc.connect(dial);
    } finally {
      void shell("settle_dial", { dial: dial.dial }).catch(() => {});
    }
    this.backoff.opened();
    this.rpc = rpc;
    rpc.onClose = () => this.lost(rpc);
    this.setState("connected");
    await this.reattach();
  }

  private lost(rpc: Rpc): void {
    if (this.rpc !== rpc) return;
    this.rpc = null;
    if (this.leaving) return;
    this.backoff.closed();
    this.setState("reconnecting");
    this.schedule();
  }

  private schedule(): void {
    window.clearTimeout(this.retry);
    this.retry = window.setTimeout(() => {
      this.connect().catch((err: unknown) => {
        console.warn("gateway dial failed:", err);
        this.backoff.failed();
        this.schedule();
      });
    }, this.backoff.delay());
  }

  private received(envelope: Envelope): void {
    const mark = envelope.session_id ? this.open.get(envelope.session_id) : undefined;
    if (mark) {
      mark.seq = envelope.seq;
      mark.epoch = envelope.epoch;
    }
    this.onEvent(envelope);
  }

  private async reattach(): Promise<void> {
    for (const [sessionId, mark] of this.open) {
      try {
        const attached = await this.call<{ truncated?: boolean; epoch: string; latest_seq: number }>("session.attach", {
          session_id: sessionId,
          last_seen_seq: mark.seq,
          epoch: mark.epoch,
        });
        mark.epoch = attached.epoch;
        if (attached.truncated) {
          mark.seq = attached.latest_seq;
          this.onTruncated(sessionId);
        }
      } catch {
        // The session is gone or no longer ours: the window shows it as
        // it was and the user decides.
      }
    }
  }

  /** Records `sessionId` as open in this window from `seq`. */
  watch(sessionId: string, seq: number | null, epoch: string | null): void {
    this.open.set(sessionId, { seq, epoch });
  }

  /** Stops following `sessionId`. */
  unwatch(sessionId: string): void {
    this.open.delete(sessionId);
  }

  /** One call over the current connection. */
  call<T>(method: string, params: unknown): Promise<T> {
    if (!this.rpc) return Promise.reject(new Error("not connected to the gateway daemon (reconnecting)"));
    return this.rpc.call(method, params) as Promise<T>;
  }

  /** A call that a dropped connection (a daemon restart) does not lose:
   *  it is sent again once redialled. Only for idempotent calls. */
  async callAcrossRestart<T>(method: string, params: unknown): Promise<T> {
    for (;;) {
      await this.ready();
      try {
        return await this.call<T>(method, params);
      } catch (err) {
        // A closed socket, or a retiring daemon (-32006) about to close it.
        const dropped = err instanceof RpcError && (err.code >= 1000 || err.code === RETIRING);
        if (!dropped && this.state === "connected") throw err;
        if (dropped && this.state === "connected") await new Promise((r) => window.setTimeout(r, 200));
      }
    }
  }

  /** Answers a server request. */
  answer(id: string, result: unknown): void {
    this.rpc?.answer(id, result);
  }

  /** Detaches the window's sessions, then dials again for `profile`; the
   *  window never holds data of two profiles at once. */
  async switchProfile(profile: string): Promise<void> {
    this.switchingProfile = true;
    try {
      for (const sessionId of this.open.keys()) {
        await this.call("session.detach", { session_id: sessionId }).catch(() => {});
      }
      this.open.clear();
      this.leaving = true;
      this.rpc?.close();
      this.rpc = null;
      this.leaving = false;
      this.profile = profile;
      this.setState("connecting");
      await this.connect();
    } finally {
      this.switchingProfile = false;
    }
  }
}
