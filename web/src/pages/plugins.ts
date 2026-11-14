// `/plugins`: the profile's plugins with their load state, and their
// dashboard contributions (spec: web-dashboard, Dashboard plugin
// contributions): each plugin's tab and slots, every override that lost
// to an earlier plugin in load order, plugins whose frames did not
// register within 2 s, and Rescan (reloads plugin discovery).

import { api, ApiError, profilePath } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { unresponsive } from "../lib/plugin-frames";
import type { Found } from "../lib/plugins";
import { canDrive, profileOf } from "../lib/state";

type Item = Record<string, unknown>;

function contributionsSection(profile: string, found: Found): HTMLElement {
  const rows = found.plugins.map((p) => [
    p.id,
    p.label,
    p.tab ? `${p.tab.path}${p.tab.hidden ? " (hidden)" : ""} at ${p.tab.position}${p.tab.override ? `, replaces ${p.tab.override}` : ""}` : "",
    p.slots.join(", "),
    p.backend ? "yes" : "no",
    unresponsive.has(p.id) ? "did not register within 2 s" : "",
  ]);
  const ignored = found.ignored.map((i) =>
    h("li", { "data-ignored": i.plugin }, `${i.plugin}: its override of ${i.route} was ignored (${i.winner} renders it, first in load order)`),
  );
  const status = h("p", { role: "status" });
  const rescan = h("button", { type: "button", disabled: !canDrive() }, "Rescan");
  rescan.addEventListener("click", async () => {
    try {
      const fresh = await api.post<Found>(`/api/plugins/rescan?profile=${encodeURIComponent(profile)}`);
      status.textContent = `Rescanned: ${fresh.plugins.length} dashboard plugin(s); reload the page to apply.`;
    } catch (err) {
      status.textContent = err instanceof ApiError ? err.message : String(err);
    }
  });
  return h(
    "section",
    { "aria-label": "Dashboard contributions" },
    h("h2", {}, "Dashboard contributions"),
    rows.length ? table(["Plugin", "Label", "Tab", "Slots", "Backend", "Problem"], rows) : notice("No plugin contributes to the dashboard."),
    ignored.length ? h("ul", { class: "ignored-overrides" }, ...ignored) : null,
    h("div", { class: "row" }, rescan),
    status,
  );
}

export async function pluginsPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  try {
    const [listed, found] = await Promise.all([
      api.get<{ plugins: Item[] }>(profilePath(profile, "/plugins")),
      api.get<Found>(`/api/plugins?profile=${encodeURIComponent(profile)}`),
    ]);
    const columns = ["id", "version", "kind", "source", "state", "problem"];
    const items = listed.plugins ?? [];
    return page(
      "Plugins",
      h("p", { class: "muted" }, `Profile ${profile}`),
      items.length ? table(columns, items.map((item) => columns.map((c) => String(item[c] ?? "")))) : notice("Nothing here yet."),
      contributionsSection(profile, found),
    );
  } catch (err) {
    return page("Plugins", notice((err as Error).message, "error"));
  }
}
