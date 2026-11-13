// Interface modes and layouts (spec: desktop-app, Interface modes and
// layouts): `advanced` (default) and `simple` (chat first: the status
// bar, terminal, file browser and review pane rest out of sight), the
// presets of each mode and the user's saved layouts. Applying a layout
// opens every pane it places and closes the rest; each mode remembers its
// own arrangement, so switching back restores it, not the preset.

import { h } from "../../../web/src/lib/dom";

export type Mode = "advanced" | "simple";

/** Where the window's parts are. */
export interface Arrangement {
  left: boolean;
  right: boolean;
  /** The right sidebar's pane. */
  pane: string;
  terminal: boolean;
}

export const PRESETS: Record<string, { mode: Mode; arrangement: Arrangement }> = {
  default: { mode: "advanced", arrangement: { left: true, right: true, pane: "settings", terminal: false } },
  basic: { mode: "advanced", arrangement: { left: true, right: false, pane: "settings", terminal: false } },
  focus: { mode: "advanced", arrangement: { left: false, right: false, pane: "settings", terminal: false } },
  "terminal-deck": { mode: "advanced", arrangement: { left: false, right: true, pane: "files", terminal: true } },
  quad: { mode: "advanced", arrangement: { left: true, right: true, pane: "files", terminal: true } },
  "sidebar-left": { mode: "simple", arrangement: { left: true, right: false, pane: "artifacts", terminal: false } },
  "sidebar-right": { mode: "simple", arrangement: { left: false, right: true, pane: "artifacts", terminal: false } },
};

const DEFAULT_OF: Record<Mode, string> = { advanced: "default", simple: "sidebar-left" };
const KEY = "prometheus.layout";

interface Stored {
  mode: Mode;
  arrangements: Partial<Record<Mode, Arrangement>>;
  saved: Record<string, { mode: Mode; arrangement: Arrangement }>;
}

function load(): Stored {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? "null") as Stored | null;
    if (raw && (raw.mode === "advanced" || raw.mode === "simple")) return { mode: raw.mode, arrangements: raw.arrangements ?? {}, saved: raw.saved ?? {} };
  } catch {
    // A fresh layout.
  }
  return { mode: "advanced", arrangements: {}, saved: {} };
}

export class Layout {
  readonly el = h("div", { class: "layout-controls" });
  private readonly modeSelect = h("select", { name: "interface-mode", "aria-label": "Interface mode" }, h("option", { value: "advanced" }, "Advanced"), h("option", { value: "simple" }, "Simple"));
  private readonly layoutSelect = h("select", { name: "layout", "aria-label": "Layout" });
  private state = load();

  constructor(private readonly apply: (mode: Mode, arrangement: Arrangement) => void) {
    this.modeSelect.addEventListener("change", () => this.setMode(this.modeSelect.value as Mode));
    this.layoutSelect.addEventListener("change", () => this.use(this.layoutSelect.value));
    const save = h("button", { type: "button", name: "save-layout" }, "Save layout");
    save.addEventListener("click", () => this.saveAs(`Layout ${Object.keys(this.state.saved).length + 1}`));
    this.el.append(this.modeSelect, this.layoutSelect, save);
  }

  get mode(): Mode {
    return this.state.mode;
  }

  get arrangement(): Arrangement {
    return this.state.arrangements[this.state.mode] ?? { ...(PRESETS[DEFAULT_OF[this.state.mode]]?.arrangement as Arrangement) };
  }

  /** Shows the remembered arrangement of the current mode. */
  start(): void {
    this.render();
    this.apply(this.state.mode, this.arrangement);
  }

  /** The user moved something: the mode remembers it. */
  change(patch: Partial<Arrangement>): void {
    this.state.arrangements[this.state.mode] = { ...this.arrangement, ...patch };
    this.persist();
    this.apply(this.state.mode, this.arrangement);
  }

  setMode(mode: Mode): void {
    this.state.mode = mode;
    this.persist();
    this.render();
    this.apply(mode, this.arrangement);
  }

  /** Applies a preset or a saved layout of the current mode. */
  use(name: string): void {
    const layout = PRESETS[name] ?? this.state.saved[name];
    if (!layout || layout.mode !== this.state.mode) return;
    this.state.arrangements[this.state.mode] = { ...layout.arrangement };
    this.persist();
    this.apply(this.state.mode, layout.arrangement);
  }

  saveAs(name: string): void {
    this.state.saved[name] = { mode: this.state.mode, arrangement: this.arrangement };
    this.persist();
    this.render();
  }

  private render(): void {
    this.modeSelect.value = this.state.mode;
    const names = [...Object.entries(PRESETS), ...Object.entries(this.state.saved)].filter(([, l]) => l.mode === this.state.mode).map(([n]) => n);
    this.layoutSelect.replaceChildren(h("option", { value: "" }, "Layout…"), ...names.map((n) => h("option", { value: n }, n)));
  }

  private persist(): void {
    try {
      localStorage.setItem(KEY, JSON.stringify(this.state));
    } catch {
      // The arrangement lasts for this window.
    }
  }
}
