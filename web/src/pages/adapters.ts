// `/adapters` (spec: web-dashboard, Channel setup forms): every adapter
// available to the profile with its messaging state, and a configure form
// rendered from the adapter's options schema. Secret fields are
// write-only (blank keeps the stored value); a save is one validated
// config patch plus the secret writes, refused as a whole naming the bad
// field. Running adapters can be paused and resumed.

import { api, ApiError, profilePath } from "../lib/api";
import { h, notice, page } from "../lib/dom";
import { canDrive, profileOf } from "../lib/state";

interface Schema {
  type?: string | string[];
  description?: string;
  properties?: Record<string, Schema>;
  required?: string[];
}

interface Adapter {
  platform: string;
  display: string;
  enabled: boolean;
  state: string;
  paused: boolean;
  message: string | null;
  options: Record<string, unknown> | null;
  schema: Schema | null;
  secrets: { name: string; set: boolean }[];
}

function kind(schema: Schema): string {
  const t = Array.isArray(schema.type) ? schema.type.find((x) => x !== "null") : schema.type;
  return t ?? "string";
}

function optionField(name: string, schema: Schema, current: unknown): HTMLElement {
  const type = kind(schema);
  if (type === "boolean") {
    return h("label", { title: schema.description ?? "" }, h("input", { type: "checkbox", name: `option:${name}`, checked: current === true }), ` ${name}`);
  }
  const value = current === undefined || current === null ? "" : typeof current === "string" ? current : JSON.stringify(current);
  return h("label", { title: schema.description ?? "" }, `${name} `, h("input", { name: `option:${name}`, value, "data-kind": type }));
}

function optionValue(input: HTMLInputElement): unknown {
  if (input.type === "checkbox") return input.checked;
  const text = input.value.trim();
  if (text === "") return null;
  const type = input.dataset.kind ?? "string";
  if (type === "string") return text;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

function form(profile: string, adapter: Adapter): HTMLFormElement {
  const status = h("div", { role: "status" });
  const props = Object.entries(adapter.schema?.properties ?? {});
  const options = props.map(([name, schema]) => optionField(name, schema, adapter.options?.[name]));
  const secrets = adapter.secrets.map((s) =>
    h("label", {}, `${s.name} `, h("input", { type: "password", name: `secret:${s.name}`, autocomplete: "off", placeholder: s.set ? "(set; blank keeps it)" : "(not set)" })),
  );
  const el = h(
    "form",
    { "aria-label": `${adapter.display} settings`, "data-adapter": adapter.platform },
    h("label", {}, h("input", { type: "checkbox", name: "enabled", checked: adapter.enabled }), " Enabled"),
    ...options,
    ...secrets,
    h("button", { type: "submit", disabled: !canDrive() }, "Save"),
    status,
  );
  el.addEventListener("submit", async (event) => {
    event.preventDefault();
    const body: { enabled: boolean; options: Record<string, unknown>; secrets: Record<string, string> } = { enabled: false, options: {}, secrets: {} };
    for (const input of el.querySelectorAll<HTMLInputElement>("input")) {
      if (input.name === "enabled") body.enabled = input.checked;
      else if (input.name.startsWith("option:")) body.options[input.name.slice(7)] = optionValue(input);
      else if (input.name.startsWith("secret:") && input.value) body.secrets[input.name.slice(7)] = input.value;
    }
    try {
      await api.put(profilePath(profile, `/adapters/${encodeURIComponent(adapter.platform)}`), body);
      status.replaceChildren(notice("Saved; it applies when the adapter restarts."));
    } catch (err) {
      status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    }
  });
  return el;
}

function pauseButton(profile: string, adapter: Adapter): HTMLElement | null {
  if (adapter.state === "disabled" || adapter.state === "stopped") return null;
  const verb = adapter.paused ? "resume" : "pause";
  const button = h("button", { type: "button", disabled: !canDrive() }, adapter.paused ? "Resume" : "Pause");
  button.addEventListener("click", async () => {
    await api.post(profilePath(profile, `/adapters/${encodeURIComponent(adapter.platform)}/${verb}`));
    button.textContent = adapter.paused ? "Pause" : "Resume";
    adapter.paused = !adapter.paused;
  });
  return button;
}

export async function adaptersPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  try {
    const data = await api.get<{ adapters: Adapter[] }>(profilePath(profile, "/adapters"));
    const cards = data.adapters.map((a) =>
      h(
        "details",
        { class: "adapter", "data-platform": a.platform },
        h("summary", {}, `${a.display}: `, h("span", { "data-state": a.state }, a.paused ? "paused" : a.state), a.message ? ` (${a.message})` : ""),
        pauseButton(profile, a),
        form(profile, a),
      ),
    );
    return page("Adapters", h("p", { class: "muted" }, `Profile ${profile}`), ...cards);
  } catch (err) {
    return page("Adapters", notice((err as Error).message, "error"));
  }
}
