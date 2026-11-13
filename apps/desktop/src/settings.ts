// Settings (spec: desktop-app, Settings write validated config patches;
// Profile switching is explicit). Every write is one `config.patch` that
// names the window's profile and carries coupled keys together; a schema
// violation leaves the file unchanged and its message shows next to the
// field.

import { h } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";

/** The subagent route: the model plus a direct endpoint's keys. */
export const ROUTE_KEYS = ["delegation.model", "delegation.base_url", "delegation.api_key_secret", "delegation.wire_format"];

export function settingsPanel(client: DesktopClient): HTMLElement {
  const route = h(
    "select",
    { name: "route", "aria-label": "Subagent route" },
    h("option", { value: "inherit" }, "Inherit from main agent"),
    h("option", { value: "model" }, "Use a model"),
  );
  const model = h("input", { name: "delegation.model", placeholder: "provider/model", "aria-label": "Subagent model" });
  const fieldError = h("p", { class: "field-error", role: "alert" });
  const status = h("p", { class: "muted", role: "status" });
  const save = h("button", { type: "submit" }, "Save");
  const form = h("form", { class: "settings-route" }, h("label", {}, "Subagents ", route), model, save, fieldError);
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    fieldError.textContent = "";
    const changes =
      route.value === "inherit"
        ? ROUTE_KEYS.map((key) => ({ key, value: null }))
        : [{ key: "delegation.model", value: model.value.trim() }];
    try {
      await client.call("config.patch", { profile: client.profile, changes });
      status.textContent = "Saved";
    } catch (err) {
      fieldError.textContent = (err as Error).message;
    }
  });
  const reload = h("button", { type: "button", name: "reload" }, "Reload configuration");
  reload.addEventListener("click", async () => {
    try {
      const state = await client.call<{ reloads: number; error?: string | null }>("config.reload", { profile: client.profile });
      status.textContent = state.error ? `Configuration invalid: ${state.error}` : `Configuration reloaded for ${client.profile}`;
    } catch (err) {
      status.textContent = (err as Error).message;
    }
  });
  return h("section", { class: "settings", "aria-label": "Settings" }, h("h2", {}, "Settings"), form, reload, status);
}
