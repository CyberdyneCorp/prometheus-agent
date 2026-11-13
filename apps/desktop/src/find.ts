// The transcript find bar (spec: desktop-app, Chat transcript rendering):
// `CommandOrControl+F` opens it; it searches the rendered transcript,
// Enter moves to the next match and Escape closes it.

import { h } from "../../../web/src/lib/dom";

export class FindBar {
  readonly el: HTMLElement;
  private readonly input = h("input", { type: "search", name: "find", placeholder: "Find in chat", "aria-label": "Find in chat" });
  private readonly count = h("span", { class: "find-count", role: "status" });
  private matches: HTMLElement[] = [];
  private current = -1;

  constructor(private readonly transcript: HTMLElement) {
    this.el = h("div", { class: "find-bar", hidden: true }, this.input, this.count);
    this.input.addEventListener("input", () => this.search());
    this.input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        this.next();
      } else if (event.key === "Escape") {
        this.close();
      }
    });
  }

  open(): void {
    this.el.hidden = false;
    this.input.focus();
    this.input.select();
    this.search();
  }

  close(): void {
    this.el.hidden = true;
    this.mark([]);
    this.count.textContent = "";
  }

  private search(): void {
    const query = this.input.value.trim().toLowerCase();
    const rows = Array.from(this.transcript.querySelectorAll<HTMLElement>(".message, details.completion"));
    this.mark(query ? rows.filter((r) => (r.textContent ?? "").toLowerCase().includes(query)) : []);
    this.current = -1;
    if (this.matches.length > 0) this.next();
    else this.count.textContent = query ? "No matches" : "";
  }

  private next(): void {
    if (this.matches.length === 0) return;
    this.matches[this.current]?.removeAttribute("data-current");
    this.current = (this.current + 1) % this.matches.length;
    const match = this.matches[this.current];
    match?.setAttribute("data-current", "true");
    match?.scrollIntoView({ block: "center" });
    this.count.textContent = `${this.current + 1} of ${this.matches.length}`;
  }

  private mark(rows: HTMLElement[]): void {
    for (const row of this.matches) {
      row.classList.remove("found");
      row.removeAttribute("data-current");
    }
    this.matches = rows;
    for (const row of rows) row.classList.add("found");
  }
}
