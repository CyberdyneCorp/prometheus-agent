// The composer's model picker (spec: desktop-app, Composer model picker):
// every provider and model the daemon's catalog reports, starred
// favourites first in starring order, the last pick remembered on this
// device and used for new chats without writing the profile's default
// (unless the profile has none: then the first pick is its default). A
// switch mid-chat lasts for the session, after a warning about the
// re-read conversation and a confirmation when the daemon flags it as
// risky. Each model's reasoning effort and fast-mode preset come back
// whenever it is picked; the pill names both levels when the route
// clamps the requested one (`Ultra→Max`).

import type { ModelOption, ModelOptions } from "../../../schema/rpc";
import { h } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { confirmIn } from "./confirm";

const LEVELS = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const FAVOURITES = "prometheus.model.favourites";

interface ModelPrefs {
  effort?: string;
  fast?: string;
}

function read<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

function write(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Device preferences only.
  }
}

const title = (level: string) => (level === "xhigh" ? "XHigh" : level.charAt(0).toUpperCase() + level.slice(1));

/** The reasoning pill: the level, or `Requested→Sent` when clamped. */
export function pill(requested: string, max: string | null | undefined): string {
  if (!max) return title(requested);
  const sent = LEVELS.indexOf(requested) > LEVELS.indexOf(max) ? max : requested;
  return sent === requested ? title(requested) : `${title(requested)}→${title(sent)}`;
}

/** Favourites in starring order, only those the catalog offers now. */
export function favouritesOf(starred: string[], models: ModelOption[]): ModelOption[] {
  return starred.flatMap((id) => models.filter((m) => m.id === id));
}

export class ModelPicker {
  readonly el = h("details", { class: "model-picker" });
  private readonly summary = h("summary", { "aria-label": "Model" });
  private readonly body = h("div", { class: "model-list" });
  private options: ModelOptions = { current: null, models: [] };
  private sessionId: string | null = null;

  constructor(
    private readonly client: DesktopClient,
    private readonly dialogs: HTMLElement,
  ) {
    this.el.append(this.summary, this.body);
  }

  private get lastKey(): string {
    return `prometheus.model.last.${this.client.profile}`;
  }

  /** The model a new chat uses: the last pick of this device. */
  get pick(): string | null {
    return read<string | null>(this.lastKey, null);
  }

  /** Lists the catalog for `sessionId` (null: the next new chat). */
  async load(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    const params = sessionId ? { session_id: sessionId } : { profile: this.client.profile };
    try {
      this.options = await this.client.call<ModelOptions>("model.options", params);
    } catch (err) {
      console.warn("model options:", err);
    }
    this.render();
  }

  /** Re-applies the model's remembered effort and fast preset to a new
   *  chat's session. */
  async applyPrefs(sessionId: string, id: string | null): Promise<void> {
    if (!id) return;
    const prefs = read<ModelPrefs>(`prometheus.model.prefs.${id}`, {});
    if (prefs.effort) await this.client.call("command.dispatch", { session_id: sessionId, name: "reasoning", args: prefs.effort }).catch(() => undefined);
    if (prefs.fast) await this.client.call("command.dispatch", { session_id: sessionId, name: "fast", args: prefs.fast }).catch(() => undefined);
  }

  private shown(): string | null {
    return this.sessionId ? (this.options.current ?? null) : (this.pick ?? this.options.current ?? null);
  }

  private render(): void {
    const shown = this.shown();
    const option = this.options.models.find((m) => m.id === shown);
    const effort = shown ? read<ModelPrefs>(`prometheus.model.prefs.${shown}`, {}).effort : undefined;
    const name = h("span", { class: "model-name" }, shown ?? "Choose a model");
    const reasoning = effort ? [h("span", { class: "reasoning-pill" }, pill(effort, option?.max_reasoning))] : [];
    this.summary.replaceChildren(name, ...reasoning);
    const starred = read<string[]>(FAVOURITES, []);
    const favourites = favouritesOf(starred, this.options.models);
    const providers = [...new Set(this.options.models.map((m) => m.provider))];
    this.body.replaceChildren(
      ...(favourites.length ? [this.section("Favourites", favourites, starred)] : []),
      ...providers.map((p) => this.section(p, this.options.models.filter((m) => m.provider === p), starred)),
      ...(option ? [this.prefs(option)] : []),
    );
  }

  private section(name: string, models: ModelOption[], starred: string[]): HTMLElement {
    const rows = models.map((m) => {
      const pickButton = h("button", { type: "button", name: "pick-model", "data-model": m.id }, m.id);
      pickButton.addEventListener("click", () => void this.choose(m));
      const star = h("button", { type: "button", name: "star", "data-model": m.id, "aria-pressed": starred.includes(m.id) ? "true" : "false" }, starred.includes(m.id) ? "★" : "☆");
      star.addEventListener("click", () => this.star(m.id));
      return h("li", { "data-model": m.id }, pickButton, star);
    });
    return h("section", { class: "model-group", "data-group": name, "aria-label": name }, h("h4", {}, name), h("ul", {}, ...rows));
  }

  private star(id: string): void {
    const starred = read<string[]>(FAVOURITES, []);
    write(FAVOURITES, starred.includes(id) ? starred.filter((s) => s !== id) : [...starred, id]);
    this.render();
  }

  /** The shown model's effort and fast preset, remembered per model. */
  private prefs(option: ModelOption): HTMLElement {
    const key = `prometheus.model.prefs.${option.id}`;
    const saved = read<ModelPrefs>(key, {});
    const effort = h("select", { name: "effort", "aria-label": "Reasoning effort" }, h("option", { value: "" }, "Default"), ...LEVELS.map((l) => h("option", { value: l, selected: saved.effort === l }, title(l))));
    const fast = h("select", { name: "fast", "aria-label": "Fast mode" }, ...["", "normal", "fast", "auto"].map((f) => h("option", { value: f, selected: (saved.fast ?? "") === f }, f || "Default")));
    const save = async () => {
      write(key, { effort: effort.value || undefined, fast: fast.value || undefined });
      if (this.sessionId) await this.applyPrefs(this.sessionId, option.id);
      this.render();
    };
    effort.addEventListener("change", () => void save());
    fast.addEventListener("change", () => void save());
    return h("div", { class: "model-prefs" }, h("label", {}, "Reasoning ", effort), h("label", {}, "Fast ", fast));
  }

  private async choose(option: ModelOption): Promise<void> {
    if (!this.sessionId) {
      write(this.lastKey, option.id);
      // A profile without a default model keeps its first pick.
      if (!this.options.current) {
        const [provider, ...rest] = option.id.split("/");
        await this.client.call("config.patch", {
          profile: this.client.profile,
          changes: [
            { key: "model.provider", value: provider },
            { key: "model.default", value: rest.join("/") },
          ],
        });
        this.options.current = option.id;
      }
      this.el.open = false;
      this.render();
      return;
    }
    await this.switchSession(this.sessionId, option);
  }

  private async switchSession(sessionId: string, option: ModelOption): Promise<void> {
    const warned = await confirmIn(this.dialogs, `Switch this chat to ${option.id}? The next message re-reads the whole conversation at full input price.`, "Switch model");
    if (!warned) return;
    if (option.risky && !(await confirmIn(this.dialogs, `Switching to ${option.id} is risky: ${option.risky}.`, "Switch anyway"))) return;
    await this.client.call("command.dispatch", { session_id: sessionId, name: "model", args: `${option.id} --session` });
    await this.applyPrefs(sessionId, option.id);
    this.el.open = false;
    await this.load(sessionId);
  }
}
