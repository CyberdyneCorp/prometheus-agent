// `/profile-builder` (spec: web-dashboard, Profile builder): a profile in
// guided steps (name and description, model and provider, persona
// `SOUL.md`, toolsets, skills, adapters), created on finish by the same
// operation as `prometheus profile create` followed by one validated
// config patch; nothing is created when a step fails validation. Cloning
// copies another profile's configuration without its secrets, sessions or
// memories.

import { api, ApiError } from "../lib/api";
import { h, notice, page } from "../lib/dom";
import { href } from "../lib/router";
import { canDrive, state } from "../lib/state";

const list = (value: FormDataEntryValue | null): string[] =>
  String(value ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "");

function step(title: string, ...fields: HTMLElement[]): HTMLElement {
  return h("fieldset", {}, h("legend", {}, title), ...fields);
}

function input(label: string, name: string, attrs: Record<string, string | boolean> = {}): HTMLElement {
  return h("label", {}, label, " ", h("input", { name, ...attrs }));
}

export function profileBuilderPage(): HTMLElement {
  const status = h("div", { role: "status" });
  const clone = h("select", { name: "clone_from", "aria-label": "Clone from" }, h("option", { value: "" }, "(start empty)"), ...state.profiles.map((p) => h("option", { value: p }, p)));
  const form = h(
    "form",
    { "aria-label": "Profile builder" },
    step("1. Name", input("Name", "name", { required: true, pattern: "[a-z0-9][a-z0-9_-]*" }), input("Description", "description"), h("label", {}, "Clone configuration from ", clone)),
    step("2. Model", input("Provider", "provider"), input("Model", "model")),
    step("3. Persona", h("label", {}, "SOUL.md ", h("textarea", { name: "soul", rows: 4 }))),
    step("4. Toolsets", input("Enabled toolsets (comma-separated)", "toolsets")),
    step("5. Skills", input("Disabled skills (comma-separated)", "skills_disabled")),
    step("6. Adapters", input("Adapters to enable (comma-separated)", "adapters")),
    h("button", { type: "submit", disabled: !canDrive() }, "Create profile"),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const data = new FormData(form);
    try {
      const created = await api.post<{ name: string }>("/api/profiles", {
        name: data.get("name"),
        description: data.get("description"),
        clone_from: data.get("clone_from") || null,
        provider: data.get("provider"),
        model: data.get("model"),
        soul: data.get("soul"),
        toolsets: list(data.get("toolsets")),
        skills_disabled: list(data.get("skills_disabled")),
        adapters: list(data.get("adapters")),
      });
      status.replaceChildren(h("p", { class: "notice info", "data-created": created.name }, `Created ${created.name}. `, h("a", { href: href(`/sessions?profile=${encodeURIComponent(created.name)}`) }, "Open it")));
    } catch (err) {
      status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    }
  });
  return page("Profile builder", status, form);
}
