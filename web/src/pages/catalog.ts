// Read-only catalog pages of the addressed profile: skills, toolsets and
// models, and machine-wide lists (profiles, adapters).

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

export async function adaptersPage(): Promise<HTMLElement> {
  const status = await api.get<{ adapters: Item[] }>("/api/status");
  return page("Adapters", rows(status.adapters, ["profile", "platform", "state", "message"]));
}
