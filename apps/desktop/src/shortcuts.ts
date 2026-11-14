// Keyboard shortcuts (spec: desktop-app, Fonts, shortcuts, palette and
// profile files; Windows, tabs and panes): every action of the window and
// every plugin-contributed one has a chord the user can rebind. The
// bindings the user changed are the machine's (`ui.prefs` keybindings);
// two actions on one chord are both flagged as conflicting until one of
// them changes. Shortcuts work in every interface mode.

import { h } from "../../../web/src/lib/dom";

/** A bindable action. */
export interface Action {
  id: string;
  label: string;
  /** Its default chord (`CommandOrControl+Shift+T`, `Ctrl+Tab`). */
  chord: string;
  run: () => void;
}

/** The chord a key press makes, in the bindings' notation. */
export function chordOf(event: KeyboardEvent): string {
  const key = event.key.length === 1 ? event.key.toUpperCase() : event.key;
  // Ctrl+Tab is Ctrl on every platform (Command+Tab belongs to the OS).
  if (key === "Tab") return `Ctrl+${event.shiftKey ? "Shift+" : ""}Tab`;
  const parts: string[] = [];
  if (event.metaKey || event.ctrlKey) parts.push("CommandOrControl");
  if (event.altKey) parts.push("Alt");
  if (event.shiftKey && key !== "+" && key !== "_") parts.push("Shift");
  parts.push(key === "_" ? "-" : key);
  return parts.join("+");
}

const MODIFIERS: Record<string, string> = {
  cmd: "CommandOrControl",
  command: "CommandOrControl",
  control: "CommandOrControl",
  cmdorctrl: "CommandOrControl",
  commandorcontrol: "CommandOrControl",
  ctrl: "Ctrl",
  shift: "Shift",
  alt: "Alt",
  tab: "Tab",
};

/** Chords as one spelling: `cmdorctrl+shift+t` reads like the default. */
export function normalize(chord: string): string {
  return chord
    .split("+")
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => MODIFIERS[part.toLowerCase()] ?? (part.length === 1 ? part.toUpperCase() : part))
    .join("+");
}

/** The actions whose chord another action also has. */
export function conflicts(chords: Map<string, string>): Set<string> {
  const byChord = new Map<string, string[]>();
  for (const [id, chord] of chords) {
    if (!chord) continue;
    byChord.set(chord, [...(byChord.get(chord) ?? []), id]);
  }
  return new Set([...byChord.values()].filter((ids) => ids.length > 1).flat());
}

export class Shortcuts {
  private readonly actions = new Map<string, Action>();
  private overrides: Record<string, string> = {};
  readonly panel = h("section", { class: "shortcuts", "aria-label": "Keyboard shortcuts" });
  /** The user changed a binding: the overrides to keep. */
  onSave: (overrides: Record<string, string>) => void = () => {};
  /** An action that must not run now (CommandOrControl+W over a terminal). */
  skip: (id: string) => boolean = () => false;

  add(action: Action): void {
    this.actions.set(action.id, action);
    this.render();
  }

  /** The bindings the machine keeps. */
  load(overrides: unknown): void {
    this.overrides = overrides && typeof overrides === "object" ? { ...(overrides as Record<string, string>) } : {};
    this.render();
  }

  chords(): Map<string, string> {
    return new Map([...this.actions.values()].map((a) => [a.id, normalize(this.overrides[a.id] ?? a.chord)]));
  }

  /** Runs the action bound to the pressed chord. */
  handle(event: KeyboardEvent): void {
    if (!(event.metaKey || event.ctrlKey) || (event.target as HTMLElement | null)?.closest?.(".shortcuts")) return;
    const pressed = chordOf(event);
    const clashing = conflicts(this.chords());
    for (const [id, chord] of this.chords()) {
      // A conflicting chord runs nothing until the user resolves it.
      if (chord !== pressed || clashing.has(id) || this.skip(id)) continue;
      event.preventDefault();
      this.actions.get(id)?.run();
      return;
    }
  }

  private bind(id: string, chord: string): void {
    const action = this.actions.get(id);
    if (!action) return;
    if (!chord || normalize(chord) === normalize(action.chord)) delete this.overrides[id];
    else this.overrides[id] = normalize(chord);
    this.onSave({ ...this.overrides });
    this.render();
  }

  private render(): void {
    const chords = this.chords();
    const clashing = conflicts(chords);
    const rows = [...this.actions.values()].map((action) => {
      const input = h("input", { "data-shortcut": action.id, value: chords.get(action.id) ?? "", "aria-label": `Shortcut for ${action.label}` });
      input.addEventListener("keydown", (event) => {
        event.stopPropagation();
        if (!(event.metaKey || event.ctrlKey) || ["Control", "Meta", "Shift", "Alt"].includes(event.key)) return;
        event.preventDefault();
        input.value = chordOf(event);
        this.bind(action.id, input.value);
      });
      input.addEventListener("change", () => this.bind(action.id, input.value));
      const conflict = clashing.has(action.id);
      const others = [...chords].filter(([id, c]) => id !== action.id && c === chords.get(action.id)).map(([id]) => this.actions.get(id)?.label ?? id);
      return h(
        "li",
        { "data-action": action.id, "data-conflict": conflict ? "true" : null },
        h("span", { class: "label" }, action.label),
        input,
        conflict ? h("span", { class: "conflict", role: "alert" }, `Conflicts with ${others.join(", ")}`) : null,
      );
    });
    this.panel.replaceChildren(h("h3", {}, "Keyboard shortcuts"), h("ul", {}, ...rows));
  }
}
