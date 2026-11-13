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
let socket: WebSocket | null = null;
let lastSeq: number | null = null;
let retry = 1000;

function open(): void {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const since = lastSeq === null ? "" : `?since=${lastSeq}`;
  socket = new WebSocket(`${scheme}://${location.host}${base}/api/events${since}`, [
    "prometheus-events",
    `csrf.${csrfToken()}`,
  ]);
  socket.onopen = () => {
    retry = 1000;
  };
  socket.onmessage = (message) => {
    const frame = JSON.parse(String(message.data));
    if (typeof frame.seq === "number") lastSeq = frame.seq;
    for (const listener of listeners) listener(frame);
  };
  socket.onclose = (closed) => {
    socket = null;
    if (closed.code === 4401 || closed.code === 4403) return;
    window.setTimeout(open, retry);
    retry = Math.min(retry * 2, 30000);
  };
}

/** Follows the stream until the returned function is called. */
export function follow(listener: Listener): () => void {
  listeners.add(listener);
  if (!socket) open();
  return () => listeners.delete(listener);
}
