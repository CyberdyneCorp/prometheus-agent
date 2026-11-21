// The Raft wake sidecar (spec: platform-adapters, Raft wake-only adapter).
// It serves 127.0.0.1:$PMT_SIDECAR_PORT. The adapter (bearer
// $PMT_SIDECAR_TOKEN) starts the Raft CLI bridge with `POST /start
// {"profile", "command"}` and drains wake hints with `POST /wakes`. The
// bridge child gets only PATH, HOME, RAFT_PROFILE and RAFT_CHANNEL_TOKEN
// (a token made per start) and posts its hints to `/wake` with that token;
// a hint carrying message content is refused and never queued.

import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import http from "node:http";
import os from "node:os";

const PORT = Number(process.env.PMT_SIDECAR_PORT);
const TOKEN = process.env.PMT_SIDECAR_TOKEN || "";
const WAKE_TOKEN = randomBytes(24).toString("hex");
const CONTENT_FIELDS = new Set(["text", "body", "content", "messages"]);
const MAX_QUEUED = 500;
const MAX_BODY = 16384;

let child = null;
const wakes = [];

// Whether `value` carries a content field at any depth.
export function carriesContent(value) {
  if (Array.isArray(value)) return value.some(carriesContent);
  if (value && typeof value === "object") {
    return Object.entries(value).some(([key, inner]) => CONTENT_FIELDS.has(key) || carriesContent(inner));
  }
  return false;
}

function startBridge({ profile, command = "raft" }) {
  if (!profile || typeof profile !== "string") return [400, { error: "profile is required" }];
  if (child) child.kill();
  const endpoint = `http://127.0.0.1:${PORT}/wake`;
  child = spawn(
    command,
    ["--profile", profile, "agent", "bridge", "--wake-adapter", "wake-channel", "--wake-channel-endpoint", endpoint],
    {
      env: {
        PATH: process.env.PATH || "",
        // The Raft CLI keeps its own login under the user's home.
        HOME: os.userInfo().homedir,
        RAFT_PROFILE: profile,
        RAFT_CHANNEL_TOKEN: WAKE_TOKEN,
      },
      stdio: ["ignore", "ignore", "inherit"],
    },
  );
  child.on("error", (error) => process.stderr.write(`raft bridge: ${error.message}\n`));
  child.on("exit", () => (child = null));
  return [200, {}];
}

function answer(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
  res.end(text);
}

function wake(req, raw, res) {
  const given = req.headers["x-raft-bridge-token"] || (req.headers.authorization || "").replace(/^Bearer /, "");
  if (given !== WAKE_TOKEN) return answer(res, 401, { error: "unauthorized" });
  let hint;
  try {
    hint = JSON.parse(raw || "{}");
  } catch {
    return answer(res, 400, { error: "not JSON" });
  }
  if (carriesContent(hint)) return answer(res, 400, { error: "wake hints carry no message content" });
  wakes.push(hint);
  if (wakes.length > MAX_QUEUED) wakes.shift();
  return answer(res, 202, {});
}

const server = http.createServer((req, res) => {
  let raw = "";
  req.setEncoding("utf8");
  req.on("data", (chunk) => {
    raw += chunk;
    if (raw.length > MAX_BODY) req.destroy();
  });
  req.on("end", () => {
    if (req.method === "POST" && req.url === "/wake") return wake(req, raw, res);
    if (req.headers.authorization !== `Bearer ${TOKEN}` || !TOKEN) return answer(res, 401, { error: "unauthorized" });
    if (req.method === "POST" && req.url === "/start") {
      try {
        const [status, body] = startBridge(JSON.parse(raw || "{}"));
        return answer(res, status, body);
      } catch (error) {
        return answer(res, 400, { error: String(error.message || error) });
      }
    }
    if (req.method === "POST" && req.url === "/wakes") return answer(res, 200, { wakes: wakes.splice(0) });
    return answer(res, 404, { error: "not found" });
  });
});

process.on("exit", () => child && child.kill());
process.on("SIGTERM", () => process.exit(0));
if (PORT) server.listen(PORT, "127.0.0.1");
