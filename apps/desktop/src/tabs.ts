// Session tabs (spec: desktop-app, Windows, tabs and panes): each tab is a
// chat (a session or a draft); `CommandOrControl+T` opens a tab,
// `Ctrl+Tab`/`Ctrl+Shift+Tab` cycle, `CommandOrControl+W` closes one and
// `CommandOrControl+Shift+T` reopens the last closed.

import { h } from "../../../web/src/lib/dom";

export class Tabs {
  readonly el = h("nav", { class: "chat-tabs", "aria-label": "Tabs" });
  /** Session ids; `""` is a draft. */
  private tabs: string[] = [""];
  private active = 0;
  private readonly closed: string[] = [];

  constructor(private readonly show: (sessionId: string | null) => void) {
    this.render();
  }

  /** The window now shows `sessionId` in the active tab. */
  showing(sessionId: string | null): void {
    this.tabs[this.active] = sessionId ?? "";
    this.render();
  }

  open(): void {
    this.tabs.push("");
    this.select(this.tabs.length - 1);
  }

  cycle(back: boolean): void {
    this.select((this.active + (back ? -1 : 1) + this.tabs.length) % this.tabs.length);
  }

  close(): void {
    this.closed.push(this.tabs[this.active] ?? "");
    this.tabs.splice(this.active, 1);
    if (this.tabs.length === 0) this.tabs.push("");
    this.select(Math.min(this.active, this.tabs.length - 1));
  }

  reopen(): void {
    const last = this.closed.pop();
    if (last === undefined) return;
    this.tabs.push(last);
    this.select(this.tabs.length - 1);
  }

  private select(index: number): void {
    this.active = index;
    this.render();
    this.show(this.tabs[index] || null);
  }

  private render(): void {
    this.el.replaceChildren(
      ...this.tabs.map((id, i) => {
        const tab = h("button", { type: "button", name: "tab", "data-tab": i, "data-session": id, "aria-selected": String(i === this.active) }, id ? id.slice(-6) : "New chat");
        tab.addEventListener("click", () => this.select(i));
        return tab;
      }),
    );
  }
}
