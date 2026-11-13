// The plugins' dashboard contributions in the shell (spec: web-dashboard,
// Dashboard plugin contributions): their tabs in the navigation at their
// declared position, their pages (a hidden tab is routable without a nav
// entry), the built-in route an override replaces, and their frames in
// the shell slots and the `<page>:top|bottom` slots of built-in pages.
//
// Contributions load after the first render and never delay it; a page
// override whose plugin has not registered within 2 s falls back to the
// built-in page, and the plugin is reported on `/plugins`.

import { api } from "./api";
import { h, notice, page } from "./dom";
import { current, render, route, routeOf, type Render } from "./router";
import { state } from "./state";
import { type Contribution, pluginFrame } from "./plugin-frames";

export interface Found {
  plugins: Contribution[];
  ignored: { plugin: string; route: string; winner: string }[];
}

export const contributions: { found: Found | null } = { found: null };

/** The shell slots; `sidebar` shows in the cockpit layout only. */
export const SHELL_SLOTS = [
  "header-left",
  "header-right",
  "header-banner",
  "sidebar",
  "pre-main",
  "post-main",
  "footer-left",
  "footer-right",
  "overlay",
];

/** `nav` with the plugins' visible tabs placed. */
export function placeTabs(nav: [string, string][], plugins: Contribution[]): [string, string][] {
  const placed = [...nav];
  for (const plugin of plugins) {
    const tab = plugin.tab;
    if (!tab || tab.hidden) continue;
    const entry: [string, string] = [tab.path, plugin.label];
    const [where, pageName] = tab.position.split(":");
    const at = placed.findIndex(([path]) => path === `/${pageName}`);
    if (where === "after" && at >= 0) placed.splice(at + 1, 0, entry);
    else if (where === "before" && at >= 0) placed.splice(at, 0, entry);
    else placed.push(entry);
  }
  return placed;
}

function pluginPage(plugin: Contribution): Render {
  return (params) => page(plugin.label, pluginFrame(plugin, "page", params.get("profile") ?? state.profile));
}

/** The plugin's page in place of a built-in one, or the built-in page
 *  when the plugin has not registered in time. */
function overridePage(plugin: Contribution, builtin: Render | undefined): Render {
  return (params) => {
    const holder = h("div", { class: "plugin-override", "data-override": plugin.id });
    const frame = pluginFrame(plugin, "page", params.get("profile") ?? state.profile, () => {
      if (!builtin) return;
      void Promise.resolve(builtin(params)).then((el) => {
        holder.replaceChildren(notice(`${plugin.label} did not respond; showing the built-in page.`, "warn"), el);
      });
    });
    holder.append(page(plugin.label, frame));
    return holder;
  };
}

function fillShell(plugins: Contribution[]): void {
  for (const slot of SHELL_SLOTS) {
    const holder = document.querySelector<HTMLElement>(`[data-shell-slot="${slot}"]`);
    if (!holder) continue;
    holder.replaceChildren(
      ...plugins.filter((p) => p.slots.includes(slot)).map((p) => pluginFrame(p, slot, state.profile)),
    );
  }
}

/** Frames of the current built-in page's top and bottom slots. */
function fillPage(plugins: Contribution[]): void {
  const main = document.getElementById("main");
  const pageEl = main?.firstElementChild;
  if (!main || !pageEl) return;
  const name = current().replace(/^\//, "") || "sessions";
  for (const place of ["top", "bottom"] as const) {
    const slot = `${name}:${place}`;
    const frames = plugins.filter((p) => p.slots.includes(slot)).map((p) => pluginFrame(p, slot, state.profile));
    if (frames.length === 0) continue;
    const holder = h("div", { class: "page-slot", "data-page-slot": slot }, ...frames);
    if (place === "top") main.prepend(holder);
    else main.append(holder);
  }
}

/** Fetches the profile's contributions and applies them; `rebuildNav`
 *  redraws the navigation with the placed tabs. */
export async function loadPlugins(rebuildNav: (plugins: Contribution[]) => void): Promise<void> {
  let found: Found;
  try {
    found = await api.get<Found>(`/api/plugins?profile=${encodeURIComponent(state.profile)}`);
  } catch {
    return;
  }
  contributions.found = found;
  for (const plugin of found.plugins) {
    const tab = plugin.tab;
    if (!tab) continue;
    route(tab.path, pluginPage(plugin));
    if (tab.override) route(tab.override, overridePage(plugin, routeOf(tab.override)));
  }
  rebuildNav(found.plugins);
  fillShell(found.plugins);
  document.addEventListener("routed", () => fillPage(found.plugins));
  await render();
}
