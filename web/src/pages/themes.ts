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
}

const KEY = "prometheus.theme";

/** Applies `name`'s style sheet (none for `default`). */
export function applyTheme(name: string): void {
  document.getElementById("theme-sheet")?.remove();
  document.documentElement.dataset.theme = name;
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

export async function themesPage(): Promise<HTMLElement> {
  const data = await api.get<{ themes: ThemeRow[]; directory: string | null }>("/api/themes");
  const status = h("div", {});
  const rows = data.themes.map((theme) => {
    const use = h("button", { type: "button", disabled: !theme.valid }, "Use");
    use.addEventListener("click", () => {
      applyTheme(theme.name);
      try {
        localStorage.setItem(KEY, theme.name);
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
