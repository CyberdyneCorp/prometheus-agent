// The chat status bar (spec: desktop-app, Desktop status bar): a
// per-session approval bypass equivalent to `/yolo` (confirmed when
// turned on, never kept past the session), the context meter whose
// popover breaks the tokens down by category, and the cache-hit rate and
// output speed readouts (off by default). Items are chosen from the bar's
// context menu; `CommandOrControl+Shift+S` hides or shows the bar.

import type { ContextUsage, Dispatched, Envelope } from "../../../schema/rpc";
import { h } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { confirmIn } from "./confirm";

const ITEMS_KEY = "prometheus.statusbar.items";
const HIDDEN_KEY = "prometheus.statusbar.hidden";
/** Output speed averages the last this many model calls. */
const SPEED_CALLS = 10;

type Item = "bypass" | "context" | "cache" | "speed";
const DEFAULT_ITEMS: Item[] = ["bypass", "context"];
const LABELS: Record<Item, string> = { bypass: "Approval bypass", context: "Context", cache: "Cache hits", speed: "Output speed" };

const CATEGORY_LABELS: Record<string, string> = {
  system_prompt: "System prompt",
  tool_definitions: "Tool definitions",
  skills: "Skills",
  memory: "Memory",
  rules: "Rules",
  mcp: "MCP",
  subagent_definitions: "Subagent definitions",
  conversation: "Conversation",
};

interface Call {
  output: number;
  ms: number;
}

function stored<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

function store(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // A display preference of this device only.
  }
}

export class StatusBar {
  readonly el = h("footer", { class: "status-bar", "aria-label": "Status bar" });
  private readonly bypass = h("input", { type: "checkbox", name: "bypass", "aria-label": "Approval bypass" });
  /** Why the last switch did not apply (a turn was running). */
  private readonly bypassNote = h("span", { class: "bypass-note", role: "status" });
  private readonly meter = h("button", { type: "button", name: "context-meter", class: "context-meter" }, "Context —");
  private readonly popover = h("div", { class: "context-popover", role: "dialog", "aria-label": "Context use", hidden: true });
  private readonly cache = h("span", { class: "readout", "data-item": "cache" }, "Cache —");
  private readonly speed = h("span", { class: "readout", "data-item": "speed" }, "— tok/s");
  private readonly menu = h("div", { class: "status-menu", role: "menu", hidden: true });
  /** Sessions whose bypass this window turned on (never stored). */
  private readonly bypassed = new Set<string>();
  private items: Item[] = stored(ITEMS_KEY, DEFAULT_ITEMS);
  private sessionId: string | null = null;
  private calls: Call[] = [];
  private cacheRead = 0;
  private prompt = 0;

  constructor(
    private readonly client: DesktopClient,
    private readonly dialogs: HTMLElement,
  ) {
    const bypassLabel = h("label", { "data-item": "bypass" }, this.bypass, " Bypass approvals", this.bypassNote);
    const context = h("span", { class: "context", "data-item": "context" }, this.meter, this.popover);
    this.el.append(bypassLabel, context, this.cache, this.speed, this.menu);
    this.el.hidden = stored(HIDDEN_KEY, false);
    this.bypass.addEventListener("change", () => void this.toggleBypass());
    this.meter.addEventListener("click", () => {
      this.popover.hidden = !this.popover.hidden;
    });
    this.el.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      this.openMenu();
    });
    client.on("session.event", (envelope) => this.observe(envelope));
    client.on("turn.finished", (envelope) => {
      if (envelope.session_id === this.sessionId) void this.refresh();
    });
    this.applyItems();
  }

  /** `CommandOrControl+Shift+S`. */
  toggle(): void {
    this.el.hidden = !this.el.hidden;
    store(HIDDEN_KEY, this.el.hidden);
  }

  /** The window now shows `sessionId` (null: a draft). */
  async follow(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    this.calls = [];
    this.cacheRead = 0;
    this.prompt = 0;
    this.popover.hidden = true;
    this.bypass.checked = sessionId !== null && this.bypassed.has(sessionId);
    this.bypass.disabled = sessionId === null;
    this.bypass.dataset.applied = String(this.bypass.checked);
    this.bypassNote.textContent = "";
    this.showReadouts();
    await this.refresh();
  }

  private async toggleBypass(): Promise<void> {
    const sessionId = this.sessionId;
    if (!sessionId) return;
    const on = this.bypass.checked;
    if (on && !(await confirmIn(this.dialogs, "Run every tool call of this session without asking for approval? This lasts until the session ends.", "Bypass approvals"))) {
      this.bypass.checked = false;
      return;
    }
    // Busy until the daemon answers: the switch says what the session
    // does only once the bypass is applied (`data-applied`).
    this.bypass.disabled = true;
    delete this.bypass.dataset.applied;
    this.bypassNote.textContent = "";
    try {
      // `/yolo` is refused while a turn runs: the switch then goes back.
      const done = await this.client.call<Dispatched>("command.dispatch", { session_id: sessionId, name: "yolo", args: on ? "on" : "off" });
      if (done.type === "refused") {
        this.bypass.checked = !on;
        this.bypassNote.textContent = ` ${done.reason}`;
      } else if (on) this.bypassed.add(sessionId);
      else this.bypassed.delete(sessionId);
    } catch (err) {
      this.bypass.checked = !on;
      console.warn("approval bypass:", err);
    }
    if (this.sessionId === sessionId) {
      this.bypass.disabled = false;
      this.bypass.dataset.applied = String(this.bypass.checked);
    }
  }

  private async refresh(): Promise<void> {
    if (!this.sessionId) {
      this.meter.textContent = "Context —";
      this.meter.dataset.tokens = "";
      this.popover.replaceChildren();
      return;
    }
    try {
      this.showUsage(await this.client.call<ContextUsage>("session.context_usage", { session_id: this.sessionId }));
    } catch (err) {
      console.warn("context usage:", err);
    }
  }

  private showUsage(usage: ContextUsage): void {
    const percent = usage.context_window ? Math.round((usage.tokens / usage.context_window) * 100) : null;
    this.meter.textContent = percent === null ? `Context ${usage.tokens} tokens` : `Context ${percent}%`;
    this.meter.dataset.tokens = String(usage.tokens);
    const rows = (usage.categories ?? []).map((c) =>
      h("li", { "data-category": c.category, "data-tokens": c.tokens }, h("span", {}, CATEGORY_LABELS[c.category] ?? c.category), h("span", { class: "tokens" }, String(c.tokens))),
    );
    this.popover.replaceChildren(
      h("ul", {}, ...rows),
      h("p", { class: "total" }, `Total ${usage.tokens} tokens${usage.context_window ? ` of ${usage.context_window}` : ""}`),
    );
  }

  /** Model calls of the shown session feed the readouts. */
  private observe(envelope: Envelope): void {
    if (envelope.session_id !== this.sessionId) return;
    const data = envelope.data as { type?: string; payload?: { latency_ms?: number; tokens?: Record<string, number> } };
    if (data.type !== "model.call" || !data.payload?.tokens) return;
    const t = data.payload.tokens;
    this.cacheRead += t.cache_read ?? 0;
    this.prompt += (t.input ?? 0) + (t.cache_read ?? 0) + (t.cache_write ?? 0);
    this.calls = [...this.calls, { output: t.output ?? 0, ms: data.payload.latency_ms ?? 0 }].slice(-SPEED_CALLS);
    this.showReadouts();
  }

  private showReadouts(): void {
    this.cache.textContent = this.prompt > 0 ? `Cache ${Math.round((this.cacheRead / this.prompt) * 100)}%` : "Cache —";
    const ms = this.calls.reduce((sum, c) => sum + c.ms, 0);
    const output = this.calls.reduce((sum, c) => sum + c.output, 0);
    this.speed.textContent = ms > 0 ? `${(output / (ms / 1000)).toFixed(1)} tok/s` : "— tok/s";
  }

  private openMenu(): void {
    const all: Item[] = ["bypass", "context", "cache", "speed"];
    this.menu.replaceChildren(
      ...all.map((item) => {
        const box = h("input", { type: "checkbox", name: `item-${item}`, checked: this.items.includes(item) });
        box.addEventListener("change", () => {
          this.items = box.checked ? [...this.items, item] : this.items.filter((i) => i !== item);
          store(ITEMS_KEY, this.items);
          this.applyItems();
        });
        return h("label", { role: "menuitemcheckbox" }, box, ` ${LABELS[item]}`);
      }),
    );
    this.menu.hidden = false;
    const close = (event: MouseEvent) => {
      if (this.menu.contains(event.target as Node)) return;
      this.menu.hidden = true;
      document.removeEventListener("click", close);
    };
    document.addEventListener("click", close);
  }

  private applyItems(): void {
    for (const el of Array.from(this.el.querySelectorAll<HTMLElement>("[data-item]"))) {
      el.hidden = !this.items.includes(el.dataset.item as Item);
    }
  }
}
