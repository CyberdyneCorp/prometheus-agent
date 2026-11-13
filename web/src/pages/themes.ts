// `/themes` (spec: web-dashboard, Dashboard themes; desktop-app,
// Machine-wide UI preferences): the built-in and user themes with the
// reason a user theme was rejected, and the light/dark mode. Using a theme
// loads its style sheet from the daemon (`/api/themes/<name>/css`, same
// origin); a rejected theme is never applied, so the current one stays.
// The choices are the machine's UI preferences, shared with every
// dashboard tab and desktop window; before the protocol connection is up
// the browser's own last choice applies.

import { api, base } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { MODES } from "../lib/prefs";
import { prefsReady, state } from "../lib/state";

interface ThemeRow {
  name: string;
  builtin: boolean;
  valid: boolean;
  error?: string;
  description?: string;
  layout_variant?: string;
}

const KEY = "prometheus.theme";
const LAYOUT_KEY = "prometheus.layout";

/** Applies `name`'s style sheet (none for `default`) and its layout
 *  variant (`standard`, `cockpit`, `tiled`). */
export function applyTheme(name: string, layout = "standard"): void {
  document.getElementById("theme-sheet")?.remove();
  document.documentElement.dataset.theme = name;
  document.documentElement.dataset.layout = layout;
  if (name === "default") return;
  const link = document.createElement("link");
  link.id = "theme-sheet";
  link.rel = "stylesheet";
  link.href = `${base}/api/themes/${encodeURIComponent(name)}/css`;
  document.head.append(link);
}

export function currentTheme(): string {
  const shared = state.prefs?.values.theme;
  if (typeof shared === "string") return shared;
  try {
    return localStorage.getItem(KEY) ?? "default";
  } catch {
    return "default";
  }
}

export function currentLayout(): string {
  const shared = state.prefs?.values.layout_variant;
  if (typeof shared === "string") return shared;
  try {
    return localStorage.getItem(LAYOUT_KEY) ?? "standard";
  } catch {
    return "standard";
  }
}

export async function themesPage(): Promise<HTMLElement> {
  // The preferences join right after sign-in; give them a moment.
  await Promise.race([prefsReady, new Promise((resolve) => window.setTimeout(resolve, 3000))]);
  const data = await api.get<{ themes: ThemeRow[]; directory: string | null }>("/api/themes");
  const status = h("div", {});
  const rows = data.themes.map((theme) => {
    const use = h("button", { type: "button", disabled: !theme.valid }, "Use");
    use.addEventListener("click", async () => {
      const layout = theme.layout_variant ?? "standard";
      applyTheme(theme.name, layout);
      try {
        localStorage.setItem(KEY, theme.name);
        localStorage.setItem(LAYOUT_KEY, layout);
      } catch {
        // A browser without storage keeps the theme for this page only.
      }
      try {
        await state.prefs?.set("theme", theme.name);
        await state.prefs?.set("layout_variant", layout);
        status.replaceChildren(notice(`Using ${theme.name}`));
      } catch (err) {
        status.replaceChildren(notice(`Using ${theme.name} in this tab only: ${(err as Error).message}`, "warn"));
      }
    });
    const shown = theme.valid ? (theme.name === currentTheme() ? "in use" : "ok") : h("span", { class: "bad", "data-rejected": theme.name }, `rejected: ${theme.error ?? ""}`);
    return [theme.name, theme.builtin ? "built-in" : "user", shown, use];
  });
  return page(
    "Themes",
    modeControl(status),
    h("p", { class: "muted" }, `User themes: ${data.directory ?? "(no home)"}/<name>.toml`),
    status,
    table(["Theme", "Source", "State", ""], rows),
  );
}

/** The light/dark mode, a machine-wide preference. */
function modeControl(status: HTMLElement): HTMLElement {
  const current = String(state.prefs?.values.mode ?? "system");
  const select = h("select", { name: "mode", "aria-label": "Mode" }, ...MODES.map((m) => h("option", { value: m, selected: m === current }, m)));
  select.disabled = state.prefs === null;
  select.addEventListener("change", async () => {
    try {
      await state.prefs?.set("mode", select.value);
      status.replaceChildren(notice(`Mode: ${select.value}`));
    } catch (err) {
      status.replaceChildren(notice((err as Error).message, "error"));
    }
  });
  return h("label", { class: "mode" }, "Mode ", select);
}
