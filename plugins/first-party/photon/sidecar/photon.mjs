// The Photon sidecar (spec: platform-adapters, Photon iMessage adapter;
// Supervised sidecar adapters). Photon's Spectrum SDK (spectrum-ts, the
// hash-pinned package of this directory's lockfile) holds the streaming
// connection to Photon; this sidecar turns it into loopback HTTP for the
// WASM adapter, on top of the socket bridge (./bridge.mjs, whose token
// check and routes it shares):
//
// - GET /inbound: inbound messages as NDJSON, one JSON object per line
//   (`{messageId, space: {id, type}, sender: {id}, content, timestamp}`);
//   attachments and voice notes are metadata only (name, MIME type, size,
//   duration), never bytes. The adapter reads it through the bridge's
//   streamed HTTP connection; lines that arrive with no reader are kept
//   (bounded) until one connects.
// - POST /send {spaceId, text, format}, /send-attachment {spaceId, name,
//   mimeType, kind, data (base64)}, /send-poll {spaceId, title, options},
//   /send-effect {spaceId, text, effect}, /typing {spaceId, state},
//   /read {messageId}: what the adapter sends; each answers {messageId}.
//
// Credentials come from the environment the host gives a sidecar: its
// declared secrets PHOTON_PROJECT_ID and PHOTON_PROJECT_SECRET.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { getRoutes, routes } from "./bridge.mjs";

const MAX_QUEUED = 10000;
const MAX_REMEMBERED = 2048;
const E164 = /^\+\d{6,15}$/;
const DM_GUID = /^any;-;(\+\d{6,})$/;

const sdk = await import("spectrum-ts");
const { imessage, effect: imessageEffect } = await import("spectrum-ts/providers/imessage");
const app = await sdk.Spectrum({
  projectId: process.env.PHOTON_PROJECT_ID,
  projectSecret: process.env.PHOTON_PROJECT_SECRET,
  providers: [imessage.config()],
  options: { flattenGroups: true },
});

const spaces = new Map();
const messages = new Map();
const queued = [];
let reader = null;

function remember(map, key, value) {
  if (!key) return;
  map.delete(key);
  map.set(key, value);
  if (map.size > MAX_REMEMBERED) map.delete(map.keys().next().value);
}

function deliver(line) {
  if (reader) {
    reader.write(line + "\n");
    return;
  }
  queued.push(line);
  if (queued.length > MAX_QUEUED) queued.shift();
}

getRoutes["/inbound"] = (req, res) => {
  res.writeHead(200, { "content-type": "application/x-ndjson" });
  reader = res;
  for (const line of queued.splice(0)) res.write(line + "\n");
  req.on("close", () => {
    if (reader === res) reader = null;
  });
};

// What the adapter learns of a message's content: text as it is, files as
// metadata only.
export function normalize(content) {
  if (!content || typeof content !== "object") return { type: "unknown" };
  switch (content.type) {
    case "text":
      return { type: "text", text: content.text || "" };
    case "richlink":
      return { type: "richlink", url: content.url || "", title: content.title ?? null };
    case "attachment":
    case "voice":
      return {
        type: content.type,
        name: content.name ?? null,
        mimeType: content.mimeType ?? null,
        size: typeof content.size === "number" ? content.size : null,
        duration: typeof content.duration === "number" ? content.duration : null,
      };
    case "group":
      return { type: "group", items: (content.items || []).map((item) => normalize(item?.content)) };
    case "reply":
      return { type: "reply", content: normalize(content.content), targetMessageId: content.target?.id ?? null };
    case "poll_option":
      return { type: "poll_option", title: content.option?.title ?? content.title ?? "", selected: content.selected !== false };
    default:
      return { type: content.type || "unknown" };
  }
}

(async () => {
  let backoff = 1000;
  for (;;) {
    try {
      for await (const [space, message] of app.messages) {
        backoff = 1000;
        if (message?.direction && message.direction !== "inbound") continue;
        remember(spaces, space?.id, space);
        remember(messages, message?.id, message);
        const ts = message.timestamp;
        deliver(
          JSON.stringify({
            messageId: message.id ?? null,
            space: { id: space?.id ?? null, type: space?.type ?? "dm" },
            sender: { id: message.sender?.id ?? null },
            content: normalize(message.content),
            timestamp: ts instanceof Date ? ts.toISOString() : ts ? String(ts) : null,
          }),
        );
      }
    } catch (error) {
      process.stderr.write(`photon: inbound stream failed: ${error?.message || error}\n`);
    }
    await new Promise((resolve) => setTimeout(resolve, backoff));
    backoff = Math.min(backoff * 2, 30000);
  }
})();

async function space(spaceId) {
  const known = spaces.get(spaceId);
  if (known) return known;
  const im = imessage(app);
  const phone = E164.test(spaceId) ? spaceId : DM_GUID.exec(spaceId)?.[1];
  const found = phone ? await im.space.create(phone) : await im.space.get(spaceId);
  if (!found) throw new Error(`no Photon conversation ${spaceId}`);
  remember(spaces, spaceId, found);
  return found;
}

const sent = (result) => [200, { messageId: result?.id ?? null }];

// A refusal keeps its meaning for the adapter's retry: a throttle stays
// 429, an unreachable or failing service 503, anything else 400.
function guarded(handler) {
  return async (body) => {
    try {
      return await handler(body);
    } catch (error) {
      const status = Number(error?.status ?? error?.statusCode ?? error?.code) || 0;
      const code = status === 429 ? 429 : status >= 500 || status === 0 ? 503 : 400;
      return [code, { error: String(error?.message || error) }];
    }
  };
}

routes["/send"] = guarded(async ({ spaceId, text, format = "markdown" }) => {
  if (!spaceId || typeof text !== "string") return [400, { error: "spaceId and text are required" }];
  const builder = format === "markdown" && !/https?:\/\//.test(text) ? sdk.markdown(text) : sdk.text(text);
  return sent(await (await space(spaceId)).send(builder));
});

routes["/send-attachment"] = guarded(async ({ spaceId, name, mimeType, kind, data }) => {
  if (!spaceId || typeof data !== "string") return [400, { error: "spaceId and data are required" }];
  const dir = mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), "photon-"));
  const base = String(name || "attachment").replace(/\\/g, "/").split("/").pop() || "attachment";
  const file = path.join(dir, base === ".." ? "attachment" : base);
  try {
    writeFileSync(file, Buffer.from(data, "base64"));
    const opts = mimeType ? { name: base, mimeType } : { name: base };
    const builder = kind === "voice" ? sdk.voice(file, opts) : sdk.attachment(file, opts);
    return sent(await (await space(spaceId)).send(builder));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

routes["/send-poll"] = guarded(async ({ spaceId, title, options }) => {
  const choices = (Array.isArray(options) ? options : []).map((o) => String(o || "").trim()).filter(Boolean);
  if (!spaceId || !title || choices.length < 2) return [400, { error: "spaceId, title and two options are required" }];
  return sent(await (await space(spaceId)).send(sdk.poll(String(title), choices)));
});

routes["/send-effect"] = guarded(async ({ spaceId, text, effect }) => {
  const id = imessage?.effect?.message?.[String(effect || "")];
  if (!spaceId || typeof text !== "string" || !id) return [400, { error: "unsupported effect" }];
  return sent(await (await space(spaceId)).send(imessageEffect(sdk.text(text), id)));
});

routes["/typing"] = guarded(async ({ spaceId, state = "start" }) => {
  if (!spaceId || (state !== "start" && state !== "stop")) return [400, { error: "spaceId and state start|stop" }];
  await (await space(spaceId)).send(sdk.typing(state));
  return [200, {}];
});

routes["/read"] = guarded(async ({ messageId }) => {
  const message = messages.get(messageId);
  if (message && typeof message.read === "function") await message.read();
  return [200, {}];
});
