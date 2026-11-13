// Plugin frames and the plugin UI SDK's message channel (spec:
// web-dashboard, Dashboard plugin contributions). A frame loads the
// plugin's document with `sandbox="allow-scripts"` and no
// `allow-same-origin`, so it runs in an opaque origin: it cannot read the
// dashboard's cookies or storage, and its own policy forbids network
// requests. Everything it needs goes through `postMessage`: `register`
// (it is ready), `context` (profile, route, slot, theme), `api` (its own
// backend routes only, and only when it has some) and `navigate`.

import { api, ApiError, base } from "./api";
import { h } from "./dom";
import { current, go } from "./router";

export interface Contribution {
  id: string;
  label: string;
  icon: string | null;
  tab: { path: string; position: string; override: string | null; hidden: boolean } | null;
  slots: string[];
  backend: boolean;
}

/** How long a frame may take to register before it is passed over. */
export const REGISTER_WITHIN_MS = 2000;

interface Mounted {
  plugin: Contribution;
  slot: string;
  profile: string;
  frame: HTMLIFrameElement;
  registered: () => void;
}

const mounted = new Set<Mounted>();
export const unresponsive = new Set<string>();
let listening = false;

type Message = { prometheus: 1; id: number; method: string; params?: Record<string, unknown> };

const METHODS = new Set(["GET", "POST", "PUT", "PATCH", "DELETE"]);

async function backendCall(entry: Mounted, params: Record<string, unknown>): Promise<unknown> {
  if (!entry.plugin.backend) throw new Error("this plugin has no backend routes");
  const method = String(params.method ?? "GET").toUpperCase();
  const path = String(params.path ?? "/");
  if (!METHODS.has(method) || !path.startsWith("/") || path.includes("..")) throw new Error("bad backend request");
  const url = `/api/plugins/${encodeURIComponent(entry.plugin.id)}${path}${path.includes("?") ? "&" : "?"}profile=${encodeURIComponent(entry.profile)}`;
  const calls: Record<string, (u: string, b?: unknown) => Promise<unknown>> = {
    GET: (u) => api.get(u),
    POST: (u, b) => api.post(u, b),
    PUT: (u, b) => api.put(u, b),
    PATCH: (u, b) => api.patch(u, b),
    DELETE: (u) => api.del(u),
  };
  return calls[method](url, params.body);
}

async function answer(entry: Mounted, message: Message): Promise<unknown> {
  switch (message.method) {
    case "register":
      entry.registered();
      return { ok: true };
    case "context":
      return {
        profile: entry.profile,
        route: current(),
        slot: entry.slot,
        theme: document.documentElement.dataset.theme ?? "default",
      };
    case "api":
      return backendCall(entry, message.params ?? {});
    case "navigate": {
      const path = String(message.params?.path ?? "");
      if (!path.startsWith("/") || path.startsWith("//")) throw new Error("navigate takes a dashboard path");
      go(path);
      return { ok: true };
    }
    default:
      throw new Error(`unknown method ${message.method}`);
  }
}

function listen(): void {
  if (listening) return;
  listening = true;
  // Frames of a page that was replaced are forgotten after each route.
  document.addEventListener("routed", () => {
    for (const entry of mounted) if (!entry.frame.isConnected) mounted.delete(entry);
  });
  window.addEventListener("message", (event) => {
    const entry = [...mounted].find((m) => event.source !== null && m.frame.contentWindow === event.source);
    const message = event.data as Message;
    if (!entry || !message || message.prometheus !== 1 || typeof message.id !== "number") return;
    const reply = (body: Record<string, unknown>) =>
      (event.source as Window).postMessage({ prometheus: 1, id: message.id, ...body }, "*");
    answer(entry, message).then(
      (result) => reply({ result }),
      (err: unknown) => reply({ error: err instanceof ApiError || err instanceof Error ? err.message : String(err) }),
    );
  });
}

/** A frame of `plugin` in `slot`; `onSilent` runs when it has not
 *  registered within [`REGISTER_WITHIN_MS`]. */
export function pluginFrame(plugin: Contribution, slot: string, profile: string, onSilent?: () => void): HTMLIFrameElement {
  listen();
  const query = `profile=${encodeURIComponent(profile)}&slot=${encodeURIComponent(slot)}`;
  const frame = h("iframe", {
    class: "plugin-frame",
    title: plugin.label,
    sandbox: "allow-scripts",
    referrerpolicy: "no-referrer",
    "data-plugin": plugin.id,
    "data-slot": slot,
    "data-state": "loading",
    src: `${base}/api/plugin-frames/${encodeURIComponent(plugin.id)}?${query}`,
  });
  let registered = false;
  const timer = window.setTimeout(() => {
    if (registered) return;
    frame.dataset.state = "unresponsive";
    unresponsive.add(plugin.id);
    onSilent?.();
  }, REGISTER_WITHIN_MS);
  mounted.add({
    plugin,
    slot,
    profile,
    frame,
    registered: () => {
      registered = true;
      window.clearTimeout(timer);
      frame.dataset.state = "registered";
      unresponsive.delete(plugin.id);
    },
  });
  return frame;
}
