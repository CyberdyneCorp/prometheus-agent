// The daemon event stream (`/api/events`): dashboard-level events of the
// served profiles (session started/ended, turn finished, approval pending,
// cron run, adapter status, budget threshold), each with a `seq`. A
// reconnect asks for what it missed with `since`; a `truncated` frame
// means some were evicted, so pages reload their data.

import { base, csrfToken } from "./api";

export interface DashboardEvent {
  seq: number;
  event: string;
  profile: string;
  session_id: string | null;
  data: Record<string, unknown>;
}

type Listener = (event: DashboardEvent | { truncated: true }) => void;

const listeners = new Set<Listener>();
const endings = new Set<() => void>();
let socket: WebSocket | null = null;
let lastSeq: number | null = null;
let retry = 1000;
/** Resolved once the current socket is open. */
let opened: Promise<void> = Promise.resolve();

function open(): void {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const since = lastSeq === null ? "" : `?since=${lastSeq}`;
  socket = new WebSocket(`${scheme}://${location.host}${base}/api/events${since}`, [
    "prometheus-events",
    `csrf.${csrfToken()}`,
  ]);
  let resolveOpened: () => void = () => {};
  opened = new Promise((resolve) => (resolveOpened = resolve));
  socket.onopen = () => {
    retry = 1000;
    resolveOpened();
  };
  socket.onmessage = (message) => {
    const frame = JSON.parse(String(message.data));
    if (typeof frame.seq === "number") lastSeq = frame.seq;
    for (const listener of listeners) listener(frame);
  };
  socket.onclose = (closed) => {
    socket = null;
    // Nobody waits forever on a socket that will not open.
    resolveOpened();
    if (closed.code === 4401) {
      for (const ended of endings) ended();
      return;
    }
    if (closed.code === 4403) return;
    window.setTimeout(open, retry);
    retry = Math.min(retry * 2, 30000);
  };
}

/** Resolves once the stream is connected: what happens after it is
 *  delivered (a step answered faster than the socket opened is not). */
export function connected(): Promise<void> {
  if (!socket) open();
  return opened;
}

/** Follows the stream until the returned function is called. */
export function follow(listener: Listener): () => void {
  listeners.add(listener);
  if (!socket) open();
  return () => listeners.delete(listener);
}

/** Calls `ended` when the daemon ends this browser's sign-in (sign-out
 *  from another tab or `prometheus webapp --sign-out`: close 4401). */
export function onSignedOut(ended: () => void): void {
  endings.add(ended);
  if (!socket) open();
}
