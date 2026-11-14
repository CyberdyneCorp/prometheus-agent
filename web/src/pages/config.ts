// `/config`: forms rendered from the config JSON Schema of the running
// build, saved as one validated write (spec: web-dashboard, Config editing
// is schema-driven). Keys that apply from the next session say so; a
// refused save shows the validator's message and changes nothing.

import { api, ApiError, profilePath } from "../lib/api";
import { docLink } from "./docs";
import { h, notice, page } from "../lib/dom";
import { profileOf } from "../lib/state";

type Schema = Record<string, unknown> & {
  $ref?: string;
  properties?: Record<string, Schema>;
  enum?: unknown[];
  oneOf?: Schema[];
  anyOf?: Schema[];
  type?: string | string[];
  description?: string;
  const?: unknown;
  "x-apply"?: string;
};

let defs: Record<string, Schema> = {};

function resolve(schema: Schema): Schema {
  let s = schema;
  for (let i = 0; i < 8 && s.$ref; i++) s = defs[s.$ref.replace("#/$defs/", "")] ?? {};
  const nonNull = (s.anyOf ?? []).filter((v) => v.type !== "null");
  if (nonNull.length === 1 && nonNull[0]) return { ...resolve(nonNull[0]), description: s.description, "x-apply": s["x-apply"] };
  return s;
}

function choices(s: Schema): string[] {
  if (s.enum) return s.enum.map(String);
  return (s.oneOf ?? []).map((v) => resolve(v)).filter((v) => v.const !== undefined || v.enum).flatMap((v) => (v.enum ?? [v.const]).map(String));
}

function typeOf(s: Schema): string {
  return Array.isArray(s.type) ? (s.type.find((t) => t !== "null") ?? "") : (s.type ?? "");
}

interface Field {
  key: string;
  input: HTMLInputElement;
  original: string;
  kind: string;
}

function field(key: string, s: Schema, value: unknown, apply: string, fields: Field[]): HTMLElement | null {
  const options = choices(s);
  const kind = options.length ? "enum" : typeOf(s);
  if (!["enum", "string", "integer", "number", "boolean"].includes(kind)) return null;
  const original = value === undefined || value === null ? "" : String(value);
  const list = options.length ? `opts-${key}` : null;
  const input = h("input", {
    name: key,
    type: kind === "boolean" ? "checkbox" : kind === "integer" || kind === "number" ? "number" : "text",
    list,
    step: kind === "number" ? "any" : null,
    "aria-label": key,
  });
  if (kind === "boolean") input.checked = value === true;
  else input.value = original;
  fields.push({ key, input, original: kind === "boolean" ? String(value === true) : original, kind });
  return h(
    "div",
    { class: "field", "data-key": key },
    h("label", {}, h("code", {}, key), input),
    list ? h("datalist", { id: list }, ...options.map((o) => h("option", { value: o }))) : null,
    s.description ? h("small", {}, s.description) : null,
    apply === "next_session" ? h("span", { class: "applies" }, "applies next session (", docLink(key), ")") : null,
    apply === "restart" ? h("span", { class: "applies" }, "applies after a daemon restart (", docLink(key), ")") : null,
  );
}

function section(name: string, s: Schema, values: Record<string, unknown>, fields: Field[]): HTMLElement | null {
  const props = s.properties ?? {};
  const rows: HTMLElement[] = [];
  for (const [key, raw] of Object.entries(props)) {
    const child = resolve(raw);
    const apply = child["x-apply"] ?? s["x-apply"] ?? "live";
    const row = field(`${name}.${key}`, child, values?.[key], apply, fields);
    if (row) rows.push(row);
  }
  return rows.length ? h("details", { "data-section": name }, h("summary", {}, name), ...rows) : null;
}

function changed(fields: Field[]): Record<string, unknown> {
  const set: Record<string, unknown> = {};
  for (const f of fields) {
    const now = f.kind === "boolean" ? String(f.input.checked) : f.input.value;
    if (now === f.original) continue;
    if (f.kind === "boolean") set[f.key] = f.input.checked;
    else if (f.kind === "integer" || f.kind === "number") set[f.key] = Number(now);
    else set[f.key] = now;
  }
  return set;
}

export async function configPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const [schema, config] = await Promise.all([
    api.get<Schema & { $defs?: Record<string, Schema> }>(profilePath(profile, "/config/schema")),
    api.get<{ values: Record<string, Record<string, unknown>> }>(profilePath(profile, "/config")),
  ]);
  defs = schema.$defs ?? {};
  const fields: Field[] = [];
  const sections = Object.entries(schema.properties ?? {})
    .map(([name, raw]) => section(name, resolve(raw), config.values[name] ?? {}, fields))
    .filter((s): s is HTMLElement => s !== null);
  const result = h("div", { class: "result" });
  // The daemon validates every value; the browser's own checks would only
  // block a save on fields hidden in closed sections.
  const form = h("form", { class: "config", novalidate: true }, ...sections, h("button", { type: "submit" }, "Save"));
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const set = changed(fields);
    if (Object.keys(set).length === 0) {
      result.replaceChildren(notice("Nothing changed."));
      return;
    }
    try {
      const saved = await api.patch<{ applies: Record<string, string> }>(profilePath(profile, "/config"), { set });
      const later = Object.entries(saved.applies).filter(([, a]) => a !== "live");
      const saidLater = later.flatMap(([k, a]) => [docLink(k), ` (${a === "next_session" ? "applies next session" : "after a restart"}) `]);
      result.replaceChildren(h("p", { class: "notice info", role: "status" }, "Saved. ", ...saidLater));
      for (const f of fields) f.original = f.kind === "boolean" ? String(f.input.checked) : f.input.value;
    } catch (err) {
      const key = err instanceof ApiError ? String(err.body.message ?? "").match(/^([a-z0-9_.]+):/)?.[1] : undefined;
      result.replaceChildren(h("p", { class: "notice error", role: "alert" }, `Not saved: ${(err as Error).message} `, key ? docLink(key) : null));
    }
  });
  return page("Configuration", h("p", { class: "muted" }, `Profile ${profile}. Lists and tables are edited with prometheus config.`), result, form);
}
