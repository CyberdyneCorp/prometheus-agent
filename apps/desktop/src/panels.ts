// Plugin UI panels (spec: desktop-app, Plugin UI contributions are
// isolated). Each panel loads in a frame on its plugin's own origin
// (`plugin-<id>://localhost`, `http://plugin-<id>.localhost` on Windows),
// so its storage is its own. The frame talks to the window only through
// the plugin UI SDK's messages, which are matched to the frame that sent
// them by window and origin, never by what the message claims, and each
// SDK call checks the capabilities granted to that plugin.

import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { shell } from "./shell";

interface Panel {
  profile: string;
  id: string;
  label: string;
  sdk: string[];
}

interface Listing {
  panels: Panel[];
  skipped: { id: string; reason: string }[];
}

interface Call {
  prometheus: 1;
  id: number;
  method: string;
  params?: Record<string, unknown>;
}

/** The origin of plugin `id`'s panel, in the form this webview uses. */
export function panelOrigin(id: string): string {
  return location.protocol === "http:" || location.protocol === "https:" ? `${location.protocol}//plugin-${id}.localhost` : `plugin-${id}://localhost`;
}

function require(panel: Panel, capability: string): void {
  if (!panel.sdk.includes(capability)) throw new Error(`capability ${capability} not granted to plugin ${panel.id}`);
}

/** A shortcut a plugin panel contributes (`register {shortcuts}`). */
export interface PluginShortcut {
  id: string;
  label: string;
  chord: string;
  run: () => void;
}

export class PanelHost {
  readonly el = h("section", { class: "panels", "aria-label": "Plugin panels" });
  private readonly frames = new Map<string, { panel: Panel; frame: HTMLIFrameElement }>();
  /** A panel contributed a bindable shortcut; pressing it tells the panel. */
  onShortcut: (shortcut: PluginShortcut) => void = () => {};

  constructor(private readonly client: DesktopClient) {
    window.addEventListener("message", (event) => void this.message(event));
  }

  /** Shows the panels of `profile`. */
  async load(profile: string): Promise<void> {
    this.frames.clear();
    let listing: Listing;
    try {
      listing = await shell<Listing>("plugin_panels", { profile });
    } catch (err) {
      this.el.replaceChildren(notice(`Plugin panels unavailable: ${String(err)}`, "warn"));
      return;
    }
    const frames = listing.panels.map((panel) => {
      const frame = h("iframe", {
        sandbox: "allow-scripts allow-same-origin",
        src: `${panelOrigin(panel.id)}/`,
        title: panel.label,
        "data-plugin": panel.id,
      }) as HTMLIFrameElement;
      this.frames.set(panel.id, { panel, frame });
      return h("figure", { class: "panel" }, h("figcaption", {}, panel.label), frame);
    });
    const skipped = listing.skipped.map((s) => notice(`Plugin ${s.id} has a panel that does not load: ${s.reason}`, "warn"));
    this.el.replaceChildren(...frames, ...skipped);
  }

  private async message(event: MessageEvent): Promise<void> {
    const entry = [...this.frames.values()].find((f) => f.frame.contentWindow === event.source);
    if (!entry || event.origin !== panelOrigin(entry.panel.id)) return;
    const call = event.data as Call;
    if (!call || call.prometheus !== 1 || typeof call.id !== "number") return;
    const reply = (body: Record<string, unknown>) => (event.source as Window).postMessage({ prometheus: 1, id: call.id, ...body }, event.origin);
    try {
      reply({ result: await this.answer(entry.panel, call) });
    } catch (err) {
      reply({ error: (err as Error).message });
    }
  }

  /** Each `{id, label, chord}` becomes a shortcut of the window that
   *  posts `{prometheus: 1, event: "shortcut", id}` to the panel. */
  private register(panel: Panel, shortcuts: unknown): void {
    if (!Array.isArray(shortcuts)) return;
    for (const s of shortcuts as { id?: unknown; label?: unknown; chord?: unknown }[]) {
      if (typeof s.id !== "string" || typeof s.chord !== "string") continue;
      const id = s.id;
      this.onShortcut({
        id: `plugin:${panel.id}:${id}`,
        label: `${panel.label}: ${typeof s.label === "string" ? s.label : id}`,
        chord: s.chord,
        run: () => this.frames.get(panel.id)?.frame.contentWindow?.postMessage({ prometheus: 1, event: "shortcut", id }, panelOrigin(panel.id)),
      });
    }
  }

  private async answer(panel: Panel, call: Call): Promise<unknown> {
    switch (call.method) {
      case "register":
        this.register(panel, call.params?.shortcuts);
        return { ok: true };
      case "context":
        return { plugin: panel.id, profile: panel.profile, theme: document.documentElement.dataset.theme ?? "default" };
      case "sessions.list": {
        require(panel, "sessions.read");
        const listed = await this.client.call<{ sessions: { session_id: string; title?: string | null }[] }>("session.list", { profile: panel.profile });
        return listed.sessions.map((s) => ({ session_id: s.session_id, title: s.title ?? null }));
      }
      default:
        // Native commands are never a plugin's: the attempt is logged.
        await shell("plugin_refused", { plugin: panel.id, command: call.method }).catch(() => {});
        throw new Error(`${call.method} is not available to plugins`);
    }
  }
}
