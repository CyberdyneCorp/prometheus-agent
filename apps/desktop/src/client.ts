// The desktop's protocol client (spec: desktop-app, One-use dial tickets;
// Shared client defaults; Reconnect backoff with full jitter; Profile
// switching is explicit). Every dial asks the shell for a fresh one-use
// ticket for the window's profile; every connection, reconnects included,
// starts with `rpc.hello` as a `desktop` client that answers server
// requests; a lost connection is redialled with full-jitter backoff and
// every session open in the window is re-attached with its last seen
// sequence number.

import type { Envelope, HelloResult } from "../../../schema/rpc";
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

export type Connection = "connecting" | "connected" | "reconnecting" | "latched";

/** A connection error the shell keeps until the user acts (a changed
 *  certificate or SSH host key): the client stops redialling. */
export interface Latch {
  kind: "certificate" | "host_key";
  message: string;
  pinned?: string;
  presented?: string;
  fingerprint?: string;
}

export class DesktopClient {
  readonly backoff = new Backoff();
  state: Connection = "connecting";
  switchingProfile = false;
  private rpc: Rpc | null = null;
  private redial: number | undefined;
  private leaving = false;
  private readonly open = new Map<string, Watermark>();
  onEvent: (envelope: Envelope) => void = () => {};
  onRequest: (request: ServerRequest) => void = () => {};
  onState: (state: Connection) => void = () => {};
  /** Every open session was re-attached after a (re)connect. */
  onReattached: () => void = () => {};
  /** A re-attached session whose missed events were evicted. */
  onTruncated: (sessionId: string) => void = () => {};
  private readonly listeners = new Map<string, Set<(envelope: Envelope) => void>>();

  /** Why dialing stopped, while latched. */
  latch: Latch | null = null;
  onLatched: (latch: Latch) => void = () => {};

  /** `connection` is a registry id (`local`: the app's own daemon);
   *  `features` go into every `rpc.hello` (the window's own client
   *  advertises `desktop_ui`, background clients nothing). */
  constructor(
    public profile: string,
    public connection = "local",
    private readonly features: string[] = [],
  ) {}

  /** The daemon's `rpc.hello` answer on the current connection. */
  get hello(): HelloResult | null {
    return this.rpc?.hello ?? null;
  }

  get reconnecting(): boolean {
    return this.state !== "connected";
  }

  /** Dials; a failed first dial keeps retrying with backoff. */
  start(): void {
    this.connect().catch((err: unknown) => void this.failed(err));
  }

  /** A failed dial: redial with backoff, unless the shell latched an
   *  error for the connection (then only the user's action redials). */
  private async failed(err: unknown): Promise<void> {
    console.warn("gateway dial failed:", err);
    this.backoff.failed();
    if (this.connection !== "local") {
      const listing = await shell<{ latched: Record<string, Latch> }>("connections_list").catch(() => ({ latched: {} as Record<string, Latch> }));
      const latch = listing.latched[this.connection];
      if (latch) {
        this.latch = latch;
        this.setState("latched");
        this.onLatched(latch);
        return;
      }
    }
    this.setState("reconnecting");
    this.schedule();
  }

  /** Dials again after the user acted on a latched error. */
  retry(): void {
    this.latch = null;
    window.clearTimeout(this.redial);
    this.setState("connecting");
    this.start();
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
    const dial = await shell<Dial>("prepare_dial", { connection: this.connection, profile: this.profile });
    const rpc = new Rpc({ ...DESKTOP, features: this.features });
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
    this.onReattached();
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
    window.clearTimeout(this.redial);
    this.redial = window.setTimeout(() => {
      this.connect().catch((err: unknown) => void this.failed(err));
    }, this.backoff.delay());
  }

  private received(envelope: Envelope): void {
    const mark = envelope.session_id ? this.open.get(envelope.session_id) : undefined;
    if (mark) {
      mark.seq = envelope.seq;
      mark.epoch = envelope.epoch;
    }
    this.onEvent(envelope);
    for (const handler of this.listeners.get(envelope.event) ?? []) handler(envelope);
  }

  /** Also calls `handler` with every `event`, across reconnects. */
  on(event: string, handler: (envelope: Envelope) => void): void {
    const set = this.listeners.get(event) ?? new Set();
    set.add(handler);
    this.listeners.set(event, set);
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
  switchProfile(profile: string): Promise<void> {
    return this.switchTo(this.connection, profile);
  }

  /** Closes the connection for good (a background connection that is no
   *  longer needed). */
  close(): void {
    this.leaving = true;
    window.clearTimeout(this.redial);
    this.rpc?.close();
    this.rpc = null;
  }

  /** Like {@link switchProfile}, to `profile` on another connection; the
   *  dial may fail, and the caller remembers only a switch that connected. */
  async switchTo(connection: string, profile: string): Promise<void> {
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
      window.clearTimeout(this.redial);
      this.latch = null;
      this.profile = profile;
      this.connection = connection;
      this.setState("connecting");
      try {
        await this.connect();
      } catch (err) {
        // Keep trying (or latch) like any failed dial; the caller learns
        // the switch did not connect.
        void this.failed(err);
        throw err;
      }
    } finally {
      this.switchingProfile = false;
    }
  }
}
