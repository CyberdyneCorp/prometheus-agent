// Fonts, zoom and the appearance overlay (spec: desktop-app, Fonts,
// shortcuts, palette and profile files). `desktop.font_family` goes in
// front of the theme's font stack and `desktop.terminal_font_family` goes
// in front of the terminal's bundled monospace font (desktop.css; empty
// uses that font alone); the interface zooms
// in half steps; a profile export can carry the appearance (theme, mode,
// accent, density, shortcuts, zoom), which an import applies again.

import type { Prefs } from "../../../web/src/lib/prefs";
import type { DesktopClient } from "./client";

/** The zoom steps: half a level each, Chromium's factor per level. */
const STEP = 0.5;
const LIMIT = 4;

/** The display preferences an exported profile carries. */
export const APPEARANCE_KEYS = ["theme", "mode", "accent", "density", "keybindings"];

/** Where the window keeps its zoom: per-window state, never a machine-wide
 *  preference. */
const ZOOM_KEY = "prometheus.zoom";

function storedZoom(): number {
  try {
    const level = Number(localStorage.getItem(ZOOM_KEY));
    return Number.isFinite(level) ? level : 0;
  } catch {
    return 0;
  }
}

/** `name` as a CSS font family. */
function family(name: string): string {
  return /^[\w-]+$/.test(name) ? name : `"${name.replace(/["\\]/g, "")}"`;
}

/** Applies the profile's font settings to the window. */
export async function applyFonts(client: DesktopClient): Promise<void> {
  const value = async (key: string) => {
    try {
      const got = (await client.call<{ value: unknown }>("config.get", { profile: client.profile, key })).value;
      return typeof got === "string" ? got.trim() : "";
    } catch {
      return "";
    }
  };
  const root = document.documentElement.style;
  const ui = await value("desktop.font_family");
  const terminal = await value("desktop.terminal_font_family");
  if (ui) root.setProperty("--ui-font", `${family(ui)},`);
  else root.removeProperty("--ui-font");
  if (terminal) root.setProperty("--terminal-font", `${family(terminal)}, "JetBrains Mono", monospace`);
  else root.removeProperty("--terminal-font");
}

/** The interface zoom of this window. */
export class Zoom {
  private level = 0;

  constructor() {
    this.apply(storedZoom());
  }

  step(direction: 1 | -1 | 0): void {
    const next = direction === 0 ? 0 : Math.max(-LIMIT, Math.min(LIMIT, this.level + direction * STEP));
    this.apply(next);
    try {
      localStorage.setItem(ZOOM_KEY, String(next));
    } catch {
      // The zoom lasts for this window.
    }
  }

  apply(level: number): void {
    this.level = level;
    document.documentElement.style.setProperty("zoom", String(1.2 ** level));
    document.documentElement.dataset.zoom = String(level);
  }
}

/** The appearance overlay of an export. */
export function snapshot(prefs: Prefs): Record<string, unknown> {
  const kept = Object.fromEntries(APPEARANCE_KEYS.filter((k) => k in prefs.values).map((k) => [k, prefs.values[k]]));
  return { ...kept, zoom: storedZoom() };
}

/** Applies an imported overlay (only display preferences are taken). */
export async function restore(prefs: Prefs, zoom: Zoom, overlay: unknown): Promise<void> {
  if (!overlay || typeof overlay !== "object") return;
  const values = overlay as Record<string, unknown>;
  for (const key of APPEARANCE_KEYS) {
    if (values[key] !== undefined) await prefs.set(key, values[key]);
  }
  if (typeof values.zoom === "number") zoom.apply(values.zoom);
}
