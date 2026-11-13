// HUD mode (spec: desktop-app, HUD mode): the focused chat detached into
// a chrome-free, always-on-top bar (`#hud=<profile>/<id>`). The bar moves
// while the user presses and holds it, resizes from its edges and goes
// back to its default size with Reset; a prompt submitted from it carries
// the metadata of the window underneath (application, title, bounds,
// never pixels). Placement features the platform cannot offer (native
// Wayland) are disabled in its settings with the reason.

import { h } from "../../../web/src/lib/dom";
import type { ChatView } from "./chat";
import { shell } from "./shell";

/** How long a press must last before the bar follows the pointer. */
const HOLD_MS = 250;

export interface WindowBelow {
  app: string;
  title: string;
  bounds: { x: number; y: number; width: number; height: number };
}

/** `text` with the window underneath named as context. */
export function withWindowBelow(text: string, below: WindowBelow | null): string {
  if (!below) return text;
  const b = below.bounds;
  return `${text}\n\n[Window under the HUD: app "${below.app}", title "${below.title}", bounds ${b.x},${b.y} ${b.width}x${b.height}; no screenshot]`;
}

/** The HUD window: the chat with the window-below context and the bar's
 *  own controls. */
export function hudWindow(chat: ChatView, center: HTMLElement): void {
  document.documentElement.dataset.window = "hud";
  chat.decorate = async (text) => withWindowBelow(text, await shell<WindowBelow | null>("window_below").catch(() => null));
  const handle = h("div", { class: "hud-handle", title: "Press and hold to move" }, "⋮⋮");
  let timer = 0;
  handle.addEventListener("pointerdown", () => {
    timer = window.setTimeout(() => void shell("hud_drag").catch(() => {}), HOLD_MS);
  });
  handle.addEventListener("pointerup", () => window.clearTimeout(timer));
  const reset = h("button", { type: "button", name: "hud-reset" }, "Reset size");
  reset.addEventListener("click", () => void shell("hud_reset"));
  center.prepend(h("div", { class: "hud-bar" }, handle, reset));
}

/** Detaches the chat into the HUD, or brings it back. */
export async function toggleHud(profile: string, sessionId: string | null): Promise<void> {
  if (sessionId) await shell("hud_toggle", { profile, session_id: sessionId });
}
