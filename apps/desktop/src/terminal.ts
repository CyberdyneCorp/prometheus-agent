// The terminal pane (spec: desktop-app, Embedded terminals are
// daemon-owned PTYs): shells the daemon serving the session spawns in its
// workspace (`pty.open`), shown with their recent output on (re)attach
// (`pty.attach`) and live `pty.output`. Only drivers type; hiding the
// pane keeps the shells running, closing a tab ends its shell, and
// selected output goes to the composer with Add to chat. Each background
// process of the session shows in a read-only tab that mirrors its output
// (spec: desktop-app, Terminal pane tools); closing that tab never stops
// the process, and `read_terminal` reads whichever tab is focused.
// One window or tab holds a terminal at a time: another one attaching
// takes it over (`pty.detached` here) until Take back attaches again. In
// the webapp a file dropped on a terminal is staged under the profile and
// its path typed into the shell. Each shell's PTY is told the character
// grid its pane shows (`pty.resize`) when it opens, is taken back, or the
// pane's size settles after a change.

import type { Envelope, ProcessList, ProcessLog, PtyAttached, PtyList, PtyOpened } from "../../../schema/rpc";
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

/** The character grid `screen` shows: its content box over one cell of
 *  its font, measured with a hidden probe. `null` while it is hidden. */
export function gridOf(screen: HTMLElement): { cols: number; rows: number } | null {
  if (!screen.isConnected || screen.clientWidth === 0) return null;
  const probe = h("span", { "aria-hidden": "true" }, "0".repeat(10));
  probe.style.cssText = "position:absolute;visibility:hidden;white-space:pre";
  screen.append(probe);
  const cell = probe.getBoundingClientRect();
  probe.remove();
  const style = getComputedStyle(screen);
  const px = (v: string) => Number.parseFloat(v) || 0;
  const width = screen.clientWidth - px(style.paddingLeft) - px(style.paddingRight);
  const height = screen.clientHeight - px(style.paddingTop) - px(style.paddingBottom);
  if (cell.width === 0 || cell.height === 0) return null;
  const clamp = (n: number) => Math.min(1000, Math.max(2, Math.floor(n)));
  return { cols: clamp(width / (cell.width / 10)), rows: clamp(height / cell.height) };
}

/** How long the pane's size must hold before the PTY is told. */
const RESIZE_SETTLE_MS = 150;

/** Most lines one `read_terminal` returns. */
const MAX_LINES = 2000;
/** How often mirrors of background processes refresh. */
const MIRROR_MS = 1000;

export class TerminalPane {
  readonly el = h("section", { class: "terminals", "aria-label": "Terminal" });
  private readonly tabs = h("div", { class: "terminal-tabs" });
  private readonly screens = h("div", { class: "terminal-screens" });
  private readonly status = h("div", { class: "terminal-status" });
  private readonly shown = new Map<string, HTMLPreElement>();
  private sessionId: string | null = null;
  private current: string | null = null;
  /** Mirror tabs by process id, and the ones the user or agent closed. */
  private readonly mirrors = new Set<string>();
  private readonly closedMirrors = new Set<string>();
  private mirrorTimer = 0;
  /** The grid each PTY was last told (`pty.resize`), so it is sent only
   *  when the pane's size really changed. */
  private readonly sizes = new Map<string, string>();
  private resizeTimer = 0;

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
    // The PTY follows the pane: a shell wraps and full-screen programs
    // draw for the grid the user sees.
    new ResizeObserver(() => {
      window.clearTimeout(this.resizeTimer);
      this.resizeTimer = window.setTimeout(() => this.fit(this.current), RESIZE_SETTLE_MS);
    }).observe(this.screens);
    client.on("pty.output", (envelope) => this.output(envelope));
    client.on("pty.exited", (envelope) => this.exited(envelope));
    client.on("pty.detached", (envelope) => {
      const data = envelope.data as { session_id: string; pty_id: string };
      if (data.session_id === this.sessionId) this.detach(data.pty_id);
    });
  }

  /** Stages a file dropped on a terminal (the webapp), typing its path. */
  onDrop: ((file: File) => Promise<string>) | null = null;

  /** Whether the keyboard is in a terminal (`CommandOrControl+W` then
   *  goes to the shell, not to the tabs). */
  get focused(): boolean {
    return this.el.contains(document.activeElement) && document.activeElement?.tagName === "PRE";
  }

  /** Shows `sessionId`'s terminals (their shells keep running). */
  async follow(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    this.shown.clear();
    this.sizes.clear();
    this.mirrors.clear();
    this.closedMirrors.clear();
    window.clearInterval(this.mirrorTimer);
    this.tabs.replaceChildren();
    this.screens.replaceChildren();
    this.status.replaceChildren();
    if (!sessionId) return;
    const listed = await this.client.call<PtyList>("pty.list", { session_id: sessionId }).catch(() => ({ terminals: [] }));
    for (const id of listed.terminals) await this.attach(id);
    this.mirrorTimer = window.setInterval(() => void this.refreshMirrors(sessionId), MIRROR_MS);
  }

  /** `read_terminal`: the focused tab's visible screen, or `count` lines
   *  of its scrollback from `start_line` (0 = oldest). */
  read(args: { start_line?: number; count?: number }): Record<string, unknown> {
    const screen = this.current ? this.shown.get(this.current) : undefined;
    if (!screen) return { success: false, error: "no terminal tab is open beside the chat" };
    const lines = (screen.textContent ?? "").replace(/\n$/, "").split("\n");
    const lineHeight = Number.parseFloat(getComputedStyle(screen).lineHeight) || 16;
    const viewport = Math.max(1, Math.floor(screen.clientHeight / lineHeight) || 24);
    const total = lines.length;
    const start = args.start_line ?? Math.max(0, total - viewport);
    const count = Math.min(MAX_LINES, args.count ?? (args.start_line === undefined ? viewport : MAX_LINES));
    const end = Math.min(total, start + count);
    return { success: true, tab: this.current, total_lines: total, start, end, viewport_rows: viewport, cursor_row: total - 1, text: lines.slice(start, end).join("\n") };
  }

  /** `close_terminal`: closes the mirror tab of a background process; the
   *  process keeps running and buffering. */
  closeMirror(id: string): Record<string, unknown> {
    // An id or a unique prefix of one, as the `process` tool takes them.
    const matches = [...this.mirrors].filter((m) => m.startsWith(id) || m.startsWith(`proc_${id}`));
    if (matches.length !== 1) {
      return { success: false, error: matches.length === 0 ? `no terminal tab mirrors process ${id}` : `${id} matches several processes: ${matches.join(", ")}` };
    }
    const processId = matches[0] ?? id;
    this.closedMirrors.add(processId);
    this.mirrors.delete(processId);
    this.drop(processId);
    return { success: true, process_id: processId, note: "the tab is closed; the process keeps running and its output keeps buffering" };
  }

  /** Opens a mirror for each new background process and refreshes the
   *  open ones. */
  private async refreshMirrors(sessionId: string): Promise<void> {
    const listed = await this.client.call<ProcessList>("process.list", { session_id: sessionId }).catch(() => ({ processes: [] }) as ProcessList);
    if (sessionId !== this.sessionId) return;
    for (const p of listed.processes) {
      if (this.closedMirrors.has(p.process_id)) continue;
      if (!this.mirrors.has(p.process_id)) this.addMirror(p.process_id, p.command);
      const log = await this.client.call<ProcessLog>("process.log", { session_id: sessionId, process_id: p.process_id }).catch(() => null);
      const screen = this.shown.get(p.process_id);
      if (log && screen && this.mirrors.has(p.process_id)) screen.textContent = log.output;
    }
  }

  private addMirror(id: string, command: string): void {
    this.mirrors.add(id);
    const screen = h("pre", { class: "terminal-screen mirror", tabindex: 0, "data-process": id, "aria-label": `Process ${id}`, "aria-readonly": "true" });
    this.shown.set(id, screen);
    const tab = h("button", { type: "button", name: "terminal-tab", "data-process": id, title: command }, `${id} (read only)`);
    tab.addEventListener("click", () => this.select(id));
    const close = h("button", { type: "button", name: "close-mirror", "data-process": id, "aria-label": `Close the ${id} mirror` }, "×");
    close.addEventListener("click", () => this.closeMirror(id));
    this.tabs.append(h("span", { class: "terminal-tab", "data-pty": id }, tab, close));
    this.screens.append(screen);
    if (!this.current) this.select(id);
    else screen.hidden = true;
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
      this.type(id, data);
    });
    screen.addEventListener("dragover", (event) => event.preventDefault());
    screen.addEventListener("drop", (event) => {
      event.preventDefault();
      for (const file of event.dataTransfer?.files ?? []) void this.dropped(id, file);
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

  private type(id: string, data: string): void {
    void this.client.call("pty.input", { session_id: this.sessionId, pty_id: id, data }).catch((err: unknown) => {
      const message = (err as Error).message;
      if (message.includes("took this terminal over")) this.detach(id);
      else this.status.replaceChildren(notice(message, "warn"));
    });
  }

  /** Another window or tab took terminal `id` over. */
  private detach(id: string): void {
    const screen = this.shown.get(id);
    if (!screen) return;
    screen.dataset.detached = "true";
    const back = h("button", { type: "button", name: "take-back", "data-pty": id }, "Take back");
    back.addEventListener("click", () => void this.takeBack(id));
    this.status.replaceChildren(notice(`${id} is open in another window or tab; Take back to type here.`, "info"), back);
  }

  private async takeBack(id: string): Promise<void> {
    const screen = this.shown.get(id);
    if (!screen) return;
    try {
      const attached = await this.client.call<PtyAttached>("pty.attach", { session_id: this.sessionId, pty_id: id });
      screen.textContent = decode(attached.replay).replace(ANSI, "");
      delete screen.dataset.detached;
      this.status.replaceChildren();
      screen.focus();
      // The other window may have sized it for itself.
      this.fit(id, true);
    } catch (err) {
      this.status.replaceChildren(notice((err as Error).message, "warn"));
    }
  }

  private async dropped(id: string, file: File): Promise<void> {
    if (!this.onDrop) return;
    try {
      const path = await this.onDrop(file);
      this.status.replaceChildren(notice(`Staged ${file.name}`, "info"));
      this.type(id, path);
    } catch (err) {
      this.status.replaceChildren(notice(`${file.name}: ${(err as Error).message}`, "warn"));
    }
  }

  private select(id: string): void {
    this.current = id;
    for (const [key, screen] of this.shown) screen.hidden = key !== id;
    this.shown.get(id)?.focus();
    this.fit(id);
  }

  /** Tells PTY `id` the grid its screen shows, when that changed (`force`:
   *  also when it did not). Mirrors of background processes have none. */
  private fit(id: string | null, force = false): void {
    const screen = id ? this.shown.get(id) : undefined;
    if (!id || !screen || !this.sessionId || this.mirrors.has(id) || screen.dataset.detached) return;
    const grid = gridOf(screen);
    if (!grid) return;
    const size = `${grid.cols}x${grid.rows}`;
    if (!force && this.sizes.get(id) === size) return;
    this.sizes.set(id, size);
    void this.client.call("pty.resize", { session_id: this.sessionId, pty_id: id, ...grid }).then(
      () => {
        screen.dataset.grid = size;
      },
      () => this.sizes.delete(id),
    );
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
    this.sizes.delete(id);
    this.tabs.querySelector(`.terminal-tab[data-pty="${id}"]`)?.remove();
    if (this.current === id) this.select([...this.shown.keys()][0] ?? "");
  }
}
