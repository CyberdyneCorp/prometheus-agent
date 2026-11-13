// History-based routing under the base path: links are plain anchors; a
// click is handled in the page and a reload of any route gets the SPA
// shell from the server.

import { base } from "./api";

type Render = (params: URLSearchParams) => HTMLElement | Promise<HTMLElement>;

const routes = new Map<string, Render>();
let mount: HTMLElement | null = null;
let fallback: Render = () => document.createElement("div");

export function route(path: string, render: Render): void {
  routes.set(path, render);
}

export function notFound(render: Render): void {
  fallback = render;
}

/** The current route path without the base path. */
export function current(): string {
  const path = location.pathname.startsWith(base) ? location.pathname.slice(base.length) : location.pathname;
  return path === "" ? "/" : path;
}

export function href(path: string): string {
  return `${base}${path}`;
}

export function go(path: string): void {
  history.pushState(null, "", href(path));
  void render();
}

export async function render(): Promise<void> {
  if (!mount) return;
  const path = current();
  const page = routes.get(path) ?? (path === "/" ? routes.get("/sessions") : undefined) ?? fallback;
  const el = await page(new URLSearchParams(location.search));
  mount.replaceChildren(el);
  document.dispatchEvent(new CustomEvent("routed", { detail: path }));
}

export function start(into: HTMLElement): void {
  mount = into;
  window.addEventListener("popstate", () => void render());
  document.addEventListener("click", (event) => {
    const anchor = (event.target as HTMLElement).closest("a");
    if (!anchor || anchor.target || event.metaKey || event.ctrlKey) return;
    const url = new URL(anchor.href, location.href);
    if (url.origin !== location.origin || !url.pathname.startsWith(base) || anchor.hasAttribute("download")) return;
    if (url.pathname.startsWith(`${base}/api/`)) return;
    event.preventDefault();
    history.pushState(null, "", url.pathname + url.search);
    void render();
  });
  void render();
}
