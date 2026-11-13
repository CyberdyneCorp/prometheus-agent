// The command palette (spec: desktop-app, Fonts, shortcuts, palette and
// profile files): `CommandOrControl+K` or `CommandOrControl+P` opens one
// box that finds pages, settings, sessions (by title or id) and actions;
// Enter runs the first match, Escape closes it.

import { h } from "../../../web/src/lib/dom";

export type EntryKind = "page" | "setting" | "session" | "action";

export interface Entry {
  kind: EntryKind;
  label: string;
  /** Matched too (a session id). */
  keywords?: string;
  run: () => void;
}

const KINDS = ["page", "setting", "session", "action"];

/** How well `entry` answers the query: an exact label first, then a
 *  label starting with it, then the rest. */
function rank(entry: Entry, words: string[]): number {
  const asked = words.filter((w) => !KINDS.includes(w)).join(" ");
  const label = entry.label.toLowerCase();
  if (label === asked) return 0;
  return label.startsWith(asked) ? 1 : 2;
}

/** The entries matching `query`, best first (otherwise in the order
 *  given). */
export function matching(entries: Entry[], query: string): Entry[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  return entries
    .filter((e) => {
      const text = `${e.kind} ${e.label} ${e.keywords ?? ""}`.toLowerCase();
      return words.every((w) => text.includes(w));
    })
    .map((entry, i) => ({ entry, i, score: rank(entry, words) }))
    .sort((a, b) => a.score - b.score || a.i - b.i)
    .map((m) => m.entry);
}

export class CommandPalette {
  private readonly input = h("input", { name: "palette-query", placeholder: "Go to a page, setting, session or action", "aria-label": "Command palette" });
  private readonly list = h("ul", { class: "palette-results", role: "listbox" });
  readonly el = h("div", { class: "command-palette", role: "dialog", "aria-label": "Command palette", hidden: true }, this.input, this.list);
  private entries: Entry[] = [];
  /** The entries of the current opening (Enter waits for them). */
  private loading: Promise<void> = Promise.resolve();

  constructor(private readonly source: () => Promise<Entry[]>) {
    this.input.addEventListener("input", () => this.render());
    this.input.addEventListener("keydown", (event) => {
      if (event.key === "Escape") this.close();
      if (event.key === "Enter") void this.loading.then(() => this.run(matching(this.entries, this.input.value)[0]));
    });
  }

  async open(): Promise<void> {
    this.el.hidden = false;
    this.input.value = "";
    this.input.focus();
    this.loading = this.source().then((entries) => {
      this.entries = entries;
      this.render();
    });
    await this.loading;
  }

  close(): void {
    this.el.hidden = true;
  }

  private run(entry: Entry | undefined): void {
    if (!entry) return;
    this.close();
    entry.run();
  }

  private render(): void {
    this.list.replaceChildren(
      ...matching(this.entries, this.input.value)
        .slice(0, 50)
        .map((entry) => {
          const item = h("li", { role: "option", "data-kind": entry.kind }, h("span", { class: "kind" }, entry.kind), ` ${entry.label}`);
          item.addEventListener("click", () => this.run(entry));
          return item;
        }),
    );
  }
}
