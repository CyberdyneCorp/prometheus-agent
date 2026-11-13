// The terminal pane (spec: desktop-app, Embedded terminals are
// daemon-owned PTYs): shells the daemon serving the session spawns in its
// workspace (`pty.open`), shown with their recent output on (re)attach
// (`pty.attach`) and live `pty.output`. Only drivers type; hiding the
// pane keeps the shells running, closing a tab ends its shell, and
// selected output goes to the composer with Add to chat.

import type { Envelope, PtyAttached, PtyList, PtyOpened } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";

// ANSI escape and control sequences the plain view does not render.
// biome-ignore lint: control characters are the point of this pattern.
const ANSI = /\u001b\[[0-?]*[ -/]*[@-~]|\u001b\][^\u0007]*\u0007|\u001b[()][A-Za-z0-9]|\r/g;

function decode(data: string): string {
  return new TextDecoder().decode(Uint8Array.from(atob(data), (c) => c.charCodeAt(0)));
}

/** What a key types into a shell. */
export function keyInput(event: KeyboardEvent): string | null {
  if (event.ctrlKey && event.key.length === 1) {
    const code = event.key.toUpperCase().charCodeAt(0);
    return code >= 64 && code <= 95 ? String.fromCharCode(code - 64) : null;
  }
  const named: Record<string, string> = { Enter: "\r", Backspace: "\x7f", Tab: "\t", Escape: "\x1b", ArrowUp: "\x1b[A", ArrowDown: "\x1b[B", ArrowRight: "\x1b[C", ArrowLeft: "\x1b[D" };
  if (named[event.key]) return named[event.key] ?? null;
  return event.key.length === 1 && !event.metaKey ? event.key : null;
}

export class TerminalPane {
  readonly el = h("section", { class: "terminals", "aria-label": "Terminal" });
  private readonly tabs = h("div", { class: "terminal-tabs" });
  private readonly screens = h("div", { class: "terminal-screens" });
  private readonly status = h("div", { class: "terminal-status" });
  private readonly shown = new Map<string, HTMLPreElement>();
  private sessionId: string | null = null;
  private current: string | null = null;

  constructor(
    private readonly client: DesktopClient,
    private readonly addToChat: (text: string) => void,
  ) {
    const open = h("button", { type: "button", name: "new-terminal" }, "New terminal");
    open.addEventListener("click", () => void this.open());
    const add = h("button", { type: "button", name: "add-to-chat" }, "Add to chat");
    add.addEventListener("click", () => {
      const selected = window.getSelection()?.toString() ?? "";
      if (selected) this.addToChat(`\`\`\`\n${selected}\n\`\`\``);
    });
    this.el.append(h("div", { class: "terminal-bar" }, open, add), this.tabs, this.screens, this.status);
    client.on("pty.output", (envelope) => this.output(envelope));
    client.on("pty.exited", (envelope) => this.exited(envelope));
  }

  /** Whether the keyboard is in a terminal (`CommandOrControl+W` then
   *  goes to the shell, not to the tabs). */
  get focused(): boolean {
    return this.el.contains(document.activeElement) && document.activeElement?.tagName === "PRE";
  }

  /** Shows `sessionId`'s terminals (their shells keep running). */
  async follow(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    this.shown.clear();
    this.tabs.replaceChildren();
    this.screens.replaceChildren();
    this.status.replaceChildren();
    if (!sessionId) return;
    const listed = await this.client.call<PtyList>("pty.list", { session_id: sessionId }).catch(() => ({ terminals: [] }));
    for (const id of listed.terminals) await this.attach(id);
  }

  private async open(): Promise<void> {
    if (!this.sessionId) {
      this.status.replaceChildren(notice("Start the chat first: a terminal opens in its workspace.", "info"));
      return;
    }
    try {
      const opened = await this.client.call<PtyOpened>("pty.open", { session_id: this.sessionId, cols: 100, rows: 30 });
      await this.attach(opened.pty_id);
    } catch (err) {
      this.status.replaceChildren(notice((err as Error).message, "error"));
    }
  }

  private async attach(id: string): Promise<void> {
    const attached = await this.client.call<PtyAttached>("pty.attach", { session_id: this.sessionId, pty_id: id });
    const screen = h("pre", { class: "terminal-screen", tabindex: 0, "data-pty": id, "aria-label": `Terminal ${id}` });
    screen.textContent = decode(attached.replay).replace(ANSI, "");
    screen.addEventListener("keydown", (event) => {
      const data = keyInput(event);
      if (data === null) return;
      event.preventDefault();
      void this.client.call("pty.input", { session_id: this.sessionId, pty_id: id, data }).catch((err: unknown) => this.status.replaceChildren(notice((err as Error).message, "warn")));
    });
    this.shown.set(id, screen);
    const tab = h("button", { type: "button", name: "terminal-tab", "data-pty": id }, id);
    tab.addEventListener("click", () => this.select(id));
    const close = h("button", { type: "button", name: "close-terminal", "data-pty": id, "aria-label": `Close ${id}` }, "×");
    close.addEventListener("click", () => void this.client.call("pty.close", { session_id: this.sessionId, pty_id: id }).then(() => this.drop(id)));
    this.tabs.append(h("span", { class: "terminal-tab", "data-pty": id }, tab, close));
    this.screens.append(screen);
    this.select(id);
  }

  private select(id: string): void {
    this.current = id;
    for (const [key, screen] of this.shown) screen.hidden = key !== id;
    this.shown.get(id)?.focus();
  }

  private output(envelope: Envelope): void {
    if (envelope.session_id !== this.sessionId) return;
    const data = envelope.data as { pty_id: string; data: string };
    const screen = this.shown.get(data.pty_id);
    if (!screen) return;
    screen.textContent = `${screen.textContent ?? ""}${decode(data.data).replace(ANSI, "")}`.slice(-200_000);
    screen.scrollTop = screen.scrollHeight;
  }

  private exited(envelope: Envelope): void {
    if (envelope.session_id === this.sessionId) this.drop((envelope.data as { pty_id: string }).pty_id);
  }

  private drop(id: string): void {
    this.shown.get(id)?.remove();
    this.shown.delete(id);
    this.tabs.querySelector(`.terminal-tab[data-pty="${id}"]`)?.remove();
    if (this.current === id) this.select([...this.shown.keys()][0] ?? "");
  }
}
