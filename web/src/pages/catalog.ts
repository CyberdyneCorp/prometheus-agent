// Read-only catalog pages of the addressed profile: skills, toolsets,
// models, plugins, MCP servers, cron jobs and memory entries, and
// machine-wide lists (profiles, adapters). Changing them from the browser
// arrives with the REST endpoint inventory (web-dashboard 61.5).

import { api, profilePath } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { href } from "../lib/router";
import { profileOf } from "../lib/state";

type Item = Record<string, unknown>;

function rows(items: Item[], columns: string[]): HTMLElement {
  if (items.length === 0) return notice("Nothing here yet.");
  return table(
    columns,
    items.map((item) => columns.map((c) => String(item[c] ?? ""))),
  );
}

async function catalog(title: string, profile: string, path: string, columns: string[]): Promise<HTMLElement> {
  try {
    const data = await api.get<Item[] | Item>(profilePath(profile, path));
    const items = Array.isArray(data) ? data : ((Object.values(data).find(Array.isArray) as Item[] | undefined) ?? []);
    return page(title, h("p", { class: "muted" }, `Profile ${profile}`), rows(items, columns));
  } catch (err) {
    return page(title, notice((err as Error).message, "error"));
  }
}

export const skillsPage = (params: URLSearchParams) => catalog("Skills", profileOf(params), "/skills", ["name", "category", "description"]);
export const toolsetsPage = (params: URLSearchParams) => catalog("Toolsets", profileOf(params), "/toolsets", ["name", "enabled", "description"]);
export const modelsPage = (params: URLSearchParams) => catalog("Models", profileOf(params), "/models", ["provider", "model", "source"]);
export const mcpPage = (params: URLSearchParams) => catalog("MCP", profileOf(params), "/mcp", ["name", "transport", "target", "trust", "enabled"]);
export const cronPage = (params: URLSearchParams) =>
  catalog("Cron", profileOf(params), "/cron", ["name", "schedule", "state", "next_run_at", "last_status"]);
export const memoryPage = (params: URLSearchParams) =>
  catalog("Memory", profileOf(params), "/memory", ["target", "tier", "kind", "status", "content"]);

export async function profilesPage(): Promise<HTMLElement> {
  const data = await api.get<{ profiles: { name: string }[] }>("/api/profiles");
  return page(
    "Profiles",
    table(
      ["Profile", ""],
      data.profiles.map((p) => [p.name, h("a", { href: href(`/sessions?profile=${encodeURIComponent(p.name)}`) }, "Sessions")]),
    ),
  );
}
