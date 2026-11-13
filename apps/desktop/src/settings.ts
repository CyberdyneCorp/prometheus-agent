// Settings (spec: desktop-app, Settings write validated config patches;
// Profile switching is explicit; Subagent route settings are drafts until
// applied; Management panes). Every write is one `config.patch` that names
// its profile and carries coupled keys together; a schema violation leaves
// the file unchanged and its message shows next to the field. The
// Subagents page (Settings → Model → Subagents) edits the route for the
// profile chosen in `Applies to` as a local draft: Apply writes it, and
// Cancel, leaving the page or another `Applies to` discards it.

import type { ModelOptions } from "../../../schema/rpc";
import { h } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";

/** The subagent route: the model plus a direct endpoint's keys. */
export const ROUTE_KEYS = ["delegation.model", "delegation.base_url", "delegation.api_key_secret", "delegation.wire_format"];

/** The route's other settings, by field name. */
const LIMIT_KEYS = ["max_iterations", "max_concurrent_children", "inactivity_timeout_secs"] as const;
const EFFORTS = ["", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

type Page = "subagents" | "general";

interface Loaded {
  model: string | null;
  limits: Record<string, string>;
  effort: string;
}

interface RouteForm {
  route: HTMLSelectElement;
  provider: HTMLSelectElement;
  model: HTMLInputElement;
  parent: HTMLInputElement;
  limits: HTMLInputElement[];
  effort: HTMLSelectElement;
}

/** The patch of an applied draft; a string is why it is refused. */
export function routePatch(loaded: Loaded, f: RouteForm): { key: string; value: unknown }[] | string {
  const changes: { key: string; value: unknown }[] = [];
  const [loadedProvider = "", ...rest] = (loaded.model ?? "").split("/");
  const loadedModel = rest.join("/");
  if (f.route.value === "inherit" || f.parent.checked) {
    if (loaded.model || f.route.value === "inherit") changes.push(...ROUTE_KEYS.map((key) => ({ key, value: null })));
  } else if (f.provider.value !== loadedProvider || f.model.value.trim() !== loadedModel) {
    if (!f.provider.value || !f.model.value.trim()) return "Choose a model for this provider, or Use parent model.";
    changes.push({ key: "delegation.model", value: `${f.provider.value}/${f.model.value.trim()}` }, ...ROUTE_KEYS.slice(1).map((key) => ({ key, value: null })));
  }
  f.limits.forEach((input, i) => {
    const key = LIMIT_KEYS[i] ?? "";
    if (input.value.trim() !== (loaded.limits[key] ?? "")) changes.push({ key: `delegation.${key}`, value: input.value.trim() ? Number(input.value) : null });
  });
  if (f.effort.value !== loaded.effort) changes.push({ key: "delegation.reasoning_effort", value: f.effort.value || null });
  return changes;
}

export class SettingsPanel {
  readonly el = h("section", { class: "settings", "aria-label": "Settings" });
  private readonly status = h("p", { class: "muted", role: "status" });
  private readonly body = h("div", { class: "settings-page" });
  private page: Page = "subagents";
  private appliesTo: string;

  constructor(
    private readonly client: DesktopClient,
    private readonly profiles: string[],
  ) {
    this.appliesTo = client.profile;
    const nav = h(
      "nav",
      { class: "settings-nav", "aria-label": "Settings pages" },
      ...(["subagents", "general"] as Page[]).map((page) => {
        const button = h("button", { type: "button", name: "settings-page", "data-page": page }, page === "subagents" ? "Model → Subagents" : "General");
        button.addEventListener("click", () => void this.show(page));
        return button;
      }),
    );
    const reload = h("button", { type: "button", name: "reload" }, "Reload configuration");
    reload.addEventListener("click", () => void this.reload());
    this.el.append(h("h2", {}, "Settings"), nav, this.body, reload, this.status);
  }

  /** The window's profile changed (or reconnected): `Applies to` resets. */
  async reset(): Promise<void> {
    this.appliesTo = this.client.profile;
    await this.show(this.page);
  }

  /** Shows `page`; whatever the previous page had not applied is gone. */
  async show(page: Page): Promise<void> {
    this.page = page;
    this.body.dataset.page = page;
    this.body.replaceChildren(page === "subagents" ? await this.subagents() : h("p", { class: "muted" }, `Profile ${this.client.profile}`));
  }

  private async reload(): Promise<void> {
    try {
      const state = await this.client.call<{ reloads: number; error?: string | null }>("config.reload", { profile: this.client.profile });
      this.status.textContent = state.error ? `Configuration invalid: ${state.error}` : `Configuration reloaded for ${this.client.profile}`;
    } catch (err) {
      this.status.textContent = (err as Error).message;
    }
  }

  private async value(key: string): Promise<unknown> {
    try {
      return (await this.client.call<{ value: unknown }>("config.get", { profile: this.appliesTo, key })).value;
    } catch {
      return null;
    }
  }

  private async current(): Promise<Loaded> {
    const model = await this.value("delegation.model");
    const limits: Record<string, string> = {};
    for (const key of LIMIT_KEYS) limits[key] = String((await this.value(`delegation.${key}`)) ?? "");
    const effort = await this.value("delegation.reasoning_effort");
    return { model: typeof model === "string" && model ? model : null, limits, effort: typeof effort === "string" ? effort : "" };
  }

  private async subagents(): Promise<HTMLElement> {
    const loaded = await this.current();
    const options = await this.client.call<ModelOptions>("model.options", { profile: this.appliesTo }).catch((): ModelOptions => ({ current: null, models: [] }));
    const [loadedProvider = "", ...rest] = (loaded.model ?? "").split("/");
    const providers = [...new Set([...options.models.map((m) => m.provider), ...(loadedProvider ? [loadedProvider] : [])])];
    const applies = h("select", { name: "applies-to", "aria-label": "Applies to" }, ...this.profiles.map((p) => h("option", { value: p, selected: p === this.appliesTo }, p)));
    applies.addEventListener("change", () => {
      this.appliesTo = applies.value;
      void this.show("subagents");
    });
    const f: RouteForm = {
      route: h(
        "select",
        { name: "route", "aria-label": "Subagent route" },
        h("option", { value: "inherit", selected: !loaded.model }, "Inherit from main agent"),
        h("option", { value: "model", selected: Boolean(loaded.model) }, "Use a model"),
      ),
      provider: h("select", { name: "provider", "aria-label": "Subagent provider" }, h("option", { value: "" }, "Provider"), ...providers.map((p) => h("option", { value: p, selected: p === loadedProvider }, p))),
      model: h("input", { name: "model", value: rest.join("/"), placeholder: "model", "aria-label": "Subagent model" }),
      parent: h("input", { type: "checkbox", name: "use-parent-model", "aria-label": "Use parent model" }),
      limits: LIMIT_KEYS.map((key) => h("input", { name: key, value: loaded.limits[key] ?? "", inputmode: "numeric", "aria-label": key })),
      effort: h("select", { name: "reasoning_effort", "aria-label": "Subagent reasoning effort" }, ...EFFORTS.map((e) => h("option", { value: e, selected: e === loaded.effort }, e || "Profile default"))),
    };
    f.provider.addEventListener("change", () => {
      f.route.value = "model";
      f.model.value = "";
    });
    const fieldError = h("p", { class: "field-error", role: "alert" });
    const cancel = h("button", { type: "button", name: "cancel" }, "Cancel");
    cancel.addEventListener("click", () => void this.show("subagents"));
    const form = h(
      "form",
      { class: "settings-route" },
      this.profiles.length >= 2 ? h("label", {}, "Applies to ", applies) : null,
      h("label", {}, "Subagents ", f.route),
      f.provider,
      f.model,
      h("label", {}, f.parent, " Use parent model"),
      ...f.limits.map((input) => h("label", {}, `${input.name} `, input)),
      h("label", {}, "Reasoning ", f.effort),
      h("button", { type: "submit" }, "Apply"),
      cancel,
      fieldError,
    );
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      fieldError.textContent = "";
      const changes = routePatch(loaded, f);
      if (typeof changes === "string") {
        fieldError.textContent = changes;
        return;
      }
      if (changes.length === 0) return;
      try {
        await this.client.call("config.patch", { profile: this.appliesTo, changes });
        this.status.textContent = "Saved";
      } catch (err) {
        fieldError.textContent = (err as Error).message;
      }
    });
    return form;
  }
}
