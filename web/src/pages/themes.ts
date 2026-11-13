// `/themes` (spec: web-dashboard, Dashboard themes): the built-in and user
// themes with the reason a user theme was rejected. Using a theme loads
// its style sheet from the daemon (`/api/themes/<name>/css`, same origin);
// a rejected theme is never applied, so the current one stays. The choice
// is remembered by this browser.

import { api, base } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";

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
  try {
    return localStorage.getItem(KEY) ?? "default";
  } catch {
    return "default";
  }
}

export function currentLayout(): string {
  try {
    return localStorage.getItem(LAYOUT_KEY) ?? "standard";
  } catch {
    return "standard";
  }
}

export async function themesPage(): Promise<HTMLElement> {
  const data = await api.get<{ themes: ThemeRow[]; directory: string | null }>("/api/themes");
  const status = h("div", {});
  const rows = data.themes.map((theme) => {
    const use = h("button", { type: "button", disabled: !theme.valid }, "Use");
    use.addEventListener("click", () => {
      const layout = theme.layout_variant ?? "standard";
      applyTheme(theme.name, layout);
      try {
        localStorage.setItem(KEY, theme.name);
        localStorage.setItem(LAYOUT_KEY, layout);
      } catch {
        // A browser without storage keeps the theme for this page only.
      }
      status.replaceChildren(notice(`Using ${theme.name}`));
    });
    const state = theme.valid ? (theme.name === currentTheme() ? "in use" : "ok") : h("span", { class: "bad", "data-rejected": theme.name }, `rejected: ${theme.error ?? ""}`);
    return [theme.name, theme.builtin ? "built-in" : "user", state, use];
  });
  return page(
    "Themes",
    h("p", { class: "muted" }, `User themes: ${data.directory ?? "(no home)"}/<name>.toml`),
    status,
    table(["Theme", "Source", "State", ""], rows),
  );
}
