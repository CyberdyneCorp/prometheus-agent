// The socket bridge of the catalog adapter plugins (spec: platform-adapters,
// Supervised sidecar adapters; Catalog adapter set). The WASM sandbox holds
// no sockets, so this sidecar keeps the adapter's long-lived connections
// (WebSocket, newline-framed TCP/TLS, streamed HTTP read line by line, and
// long-poll HTTP requests answered as one frame) and buffers what arrives
// until the adapter's next poll fetches it.
//
// It serves 127.0.0.1:$PMT_SIDECAR_PORT, answers only requests bearing
// $PMT_SIDECAR_TOKEN, and connects only to hosts in $PMT_SIDECAR_NETWORK
// (the plugin's network grant: `host`, `*.suffix` or `*`, each with an
// optional `:port`), redirects included: an HTTP redirect is followed only
// to a granted host. No dependencies: Node 22+ has WebSocket and fetch.

import http from "node:http";
import net from "node:net";
import tls from "node:tls";

const PORT = Number(process.env.PMT_SIDECAR_PORT);
const TOKEN = process.env.PMT_SIDECAR_TOKEN || "";
const GRANTS = (process.env.PMT_SIDECAR_NETWORK || "")
  .split(",")
  .map((entry) => entry.trim())
  .filter(Boolean);
// Frames kept per connection before the oldest are dropped (an adapter
// that stopped polling must not grow the bridge without bound).
const MAX_BUFFERED = 10000;
// Redirects followed per request.
const MAX_REDIRECTS = 5;

const connections = new Map();
let nextId = 1;

function defaultPort(protocol) {
  return { "ws:": 80, "http:": 80, "wss:": 443, "https:": 443 }[protocol];
}

// Whether a grant entry covers host:port.
function covers(entry, host, port) {
  let pattern = entry;
  let grantPort = null;
  const colon = entry.lastIndexOf(":");
  if (colon > 0 && /^\d+$/.test(entry.slice(colon + 1))) {
    pattern = entry.slice(0, colon);
    grantPort = Number(entry.slice(colon + 1));
  }
  if (grantPort !== null && grantPort !== port) return false;
  if (pattern === "*") return true;
  if (pattern.startsWith("*.")) return host.endsWith(pattern.slice(1));
  return pattern.toLowerCase() === host.toLowerCase();
}

export function permitted(grants, host, port) {
  return grants.some((entry) => covers(entry, host, port));
}

function target(url) {
  const parsed = new URL(url);
  const host = parsed.hostname.replace(/^\[|\]$/g, "");
  const port = Number(parsed.port) || defaultPort(parsed.protocol);
  return { parsed, host, port };
}

// Headers that must not follow a redirect to another origin.
const CREDENTIAL_HEADERS = ["authorization", "cookie", "proxy-authorization"];

function withoutCredentials(headers) {
  return Object.fromEntries(
    Object.entries(headers || {}).filter(([name]) => !CREDENTIAL_HEADERS.includes(name.toLowerCase())),
  );
}

// fetch, following redirects by hand so that each hop is checked against
// the grant like the first URL (a granted host, or an open redirect on it,
// must not lead the bridge to any other host the OS lets it reach).
export async function guardedFetch(url, init, grants = GRANTS) {
  let current = new URL(url);
  let options = { ...init, redirect: "manual" };
  for (let hop = 0; ; hop++) {
    const response = await fetch(current, options);
    const location = response.headers.get("location");
    if (response.status < 300 || response.status > 399 || !location) return response;
    await response.body?.cancel();
    if (hop >= MAX_REDIRECTS) throw new Error(`more than ${MAX_REDIRECTS} redirects`);
    const next = new URL(location, current);
    const where = target(next.href);
    if (!permitted(grants, where.host, where.port)) {
      throw new Error(`redirected to ${where.host}:${where.port}, which the plugin may not connect to`);
    }
    if (next.origin !== current.origin) options = { ...options, headers: withoutCredentials(options.headers) };
    const method = (options.method || "GET").toUpperCase();
    if (response.status === 303 || ([301, 302].includes(response.status) && method === "POST")) {
      options = { ...options, method: "GET", body: undefined };
    }
    current = next;
  }
}

function record() {
  return { frames: [], open: true, error: null, send: null, close: null };
}

function push(conn, frame) {
  conn.frames.push(frame);
  if (conn.frames.length > MAX_BUFFERED) conn.frames.shift();
}

function failed(conn, error) {
  conn.open = false;
  if (error && !conn.error) conn.error = String(error.message || error);
}

// With `binary`, binary frames cross as base64 both ways (protobuf
// protocols); otherwise they are read as UTF-8 text.
function openWebSocket(conn, url, headers, binary) {
  const socket = new WebSocket(url, { headers });
  socket.binaryType = "arraybuffer";
  socket.addEventListener("message", (event) => {
    if (typeof event.data === "string") return push(conn, event.data);
    push(conn, Buffer.from(event.data).toString(binary ? "base64" : "utf8"));
  });
  socket.addEventListener("close", (event) => {
    failed(conn, event.code === 1000 ? null : `closed with ${event.code} ${event.reason || ""}`.trim());
  });
  socket.addEventListener("error", (event) => failed(conn, event.message || "websocket error"));
  conn.send = (text) => {
    const data = binary ? Buffer.from(text, "base64") : text;
    if (socket.readyState === WebSocket.CONNECTING) {
      socket.addEventListener("open", () => socket.send(data), { once: true });
    } else {
      socket.send(data);
    }
  };
  conn.close = () => socket.close(1000);
}

function openLines(conn, stream) {
  let pending = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => {
    pending += chunk;
    let newline;
    while ((newline = pending.indexOf("\n")) >= 0) {
      push(conn, pending.slice(0, newline).replace(/\r$/, ""));
      pending = pending.slice(newline + 1);
    }
  });
  stream.on("error", (error) => failed(conn, error));
  stream.on("close", () => failed(conn, null));
}

function openSocket(conn, { parsed, host, port }) {
  const secure = parsed.protocol === "tls:";
  const stream = secure ? tls.connect({ host, port, servername: host }) : net.connect({ host, port });
  openLines(conn, stream);
  conn.send = (data) => stream.write(data);
  conn.close = () => stream.end();
}

// One HTTP request (a long poll) whose whole answer becomes a single frame,
// `{"status", "body"}`, after which the connection closes.
async function openRequest(conn, url, headers, method, body) {
  const controller = new AbortController();
  conn.close = () => controller.abort();
  conn.send = () => {
    throw new Error("a request connection takes no frames");
  };
  try {
    const response = await guardedFetch(url, { method, headers, body, signal: controller.signal });
    push(conn, JSON.stringify({ status: response.status, body: await response.text() }));
    failed(conn, null);
  } catch (error) {
    failed(conn, controller.signal.aborted ? null : error);
  }
}

async function openStream(conn, url, headers) {
  const controller = new AbortController();
  conn.close = () => controller.abort();
  conn.send = () => {
    throw new Error("a streamed HTTP connection is read-only");
  };
  try {
    const response = await guardedFetch(url, { headers, signal: controller.signal });
    if (!response.ok) {
      failed(conn, `status ${response.status}`);
      return;
    }
    const decoder = new TextDecoder();
    let pending = "";
    for await (const chunk of response.body) {
      pending += decoder.decode(chunk, { stream: true });
      let newline;
      while ((newline = pending.indexOf("\n")) >= 0) {
        const line = pending.slice(0, newline).replace(/\r$/, "");
        if (line) push(conn, line);
        pending = pending.slice(newline + 1);
      }
    }
    failed(conn, null);
  } catch (error) {
    failed(conn, controller.signal.aborted ? null : error);
  }
}

function open({ url, headers = {}, method, body, whole = false, binary = false }) {
  const where = target(url);
  if (!permitted(GRANTS, where.host, where.port)) {
    return [403, { error: `the plugin may not connect to ${where.host}:${where.port}` }];
  }
  const conn = record();
  const id = `c${nextId++}`;
  connections.set(id, conn);
  const kind = where.parsed.protocol;
  if (kind === "ws:" || kind === "wss:") openWebSocket(conn, url, headers, binary);
  else if (kind === "tcp:" || kind === "tls:") openSocket(conn, where);
  else if ((kind === "http:" || kind === "https:") && whole) openRequest(conn, url, headers, method || "GET", body);
  else if (kind === "http:" || kind === "https:") openStream(conn, url, headers);
  else {
    connections.delete(id);
    return [400, { error: `unsupported scheme ${kind}` }];
  }
  return [200, { id }];
}

function find(id) {
  const conn = connections.get(id);
  return conn ? [conn, null] : [null, [404, { error: `no connection ${id}` }]];
}

// GET routes a sidecar built on this module adds, each answering on the
// raw response (Photon's NDJSON inbound stream); none of the bridge's own.
export const getRoutes = {};

// The POST routes; a sidecar that imports this module may add its own
// (an answer may be a promise).
export const routes = {
  "/open": (body) => open(body),
  "/send": (body) => {
    const [conn, missing] = find(body.id);
    if (missing) return missing;
    if (!conn.open) return [409, { error: conn.error || "closed" }];
    conn.send(String(body.data ?? ""));
    return [200, {}];
  },
  "/recv": (body) => {
    const [conn, missing] = find(body.id);
    if (missing) return missing;
    const frames = conn.frames.splice(0, Math.max(1, Number(body.max) || 500));
    if (!conn.open && conn.frames.length === 0 && frames.length === 0) connections.delete(body.id);
    return [200, { frames, open: conn.open, error: conn.error }];
  },
  "/close": (body) => {
    const [conn, missing] = find(body.id);
    if (missing) return [200, {}];
    connections.delete(body.id);
    try {
      conn.close();
    } catch {
      // Already closed.
    }
    return [200, {}];
  },
};

function answer(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
  res.end(text);
}

const server = http.createServer((req, res) => {
  if (req.headers.authorization !== `Bearer ${TOKEN}` || !TOKEN) return answer(res, 401, { error: "unauthorized" });
  if (req.method === "GET" && getRoutes[req.url]) return getRoutes[req.url](req, res);
  const route = req.method === "POST" ? routes[req.url] : undefined;
  if (!route) return answer(res, 404, { error: "not found" });
  let raw = "";
  req.setEncoding("utf8");
  req.on("data", (chunk) => (raw += chunk));
  req.on("end", () => {
    Promise.resolve()
      .then(() => route(raw ? JSON.parse(raw) : {}))
      .then(
        ([status, body]) => answer(res, status, body),
        (error) => answer(res, 400, { error: String(error.message || error) }),
      );
  });
});

if (PORT) server.listen(PORT, "127.0.0.1");
