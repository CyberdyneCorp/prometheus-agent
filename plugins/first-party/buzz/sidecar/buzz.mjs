// The Buzz sidecar (spec: platform-adapters, Buzz Nostr adapter): the
// socket bridge (the relay WebSocket) plus `POST /cli`, which runs the Buzz
// CLI for the adapter: `{"command", "args", "input", "relay", "file"?}` ->
// `{"code", "stdout", "stderr"}`. The private key reaches the CLI only
// through its environment (BUZZ_PRIVATE_KEY, which the host hands this
// sidecar as its one declared secret), never through argv; arguments are
// passed as a list (no shell). A file to send is written to a fresh
// directory under $TMPDIR (the sidecar's data directory) and named with
// `--file`, then removed.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import os from "node:os";
import { routes } from "./bridge.mjs";

const KEY = process.env.BUZZ_PRIVATE_KEY || "";
const TIMEOUT_MS = 30000;
const MAX_OUTPUT = 1024 * 1024;

function safeName(name) {
  const base = String(name || "attachment").replace(/\\/g, "/").split("/").pop();
  return base && base !== "." && base !== ".." ? base : "attachment";
}

export function run({ command = "buzz", args = [], input = "", relay = "", file = null }) {
  if (!Array.isArray(args) || !args.every((a) => typeof a === "string")) {
    return [400, { error: "args must be a list of strings" }];
  }
  let dir = null;
  const argv = [...args];
  if (file) {
    dir = mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), "buzz-"));
    const target = path.join(dir, safeName(file.name));
    writeFileSync(target, Buffer.from(String(file.data || ""), "base64"));
    argv.push("--file", target);
  }
  return new Promise((resolve) => {
    const env = { PATH: process.env.PATH || "", HOME: process.env.HOME || os.homedir(), BUZZ_PRIVATE_KEY: KEY };
    if (relay) env.BUZZ_RELAY_URL = relay;
    const child = spawn(String(command), argv, { env, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => child.kill("SIGKILL"), TIMEOUT_MS);
    child.stdout.on("data", (chunk) => (stdout = (stdout + chunk).slice(0, MAX_OUTPUT)));
    child.stderr.on("data", (chunk) => (stderr = (stderr + chunk).slice(0, MAX_OUTPUT)));
    const done = (code) => {
      clearTimeout(timer);
      if (dir) rmSync(dir, { recursive: true, force: true });
      resolve([200, { code, stdout, stderr }]);
    };
    child.on("error", (error) => {
      stderr += error.message;
      done(127);
    });
    child.on("close", (code) => done(code ?? 1));
    child.stdin.end(String(input || ""));
  });
}

routes["/cli"] = run;
