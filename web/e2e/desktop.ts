// The desktop app's page in the browser harness (crates/prometheus/tests/
// common/desktop.rs): served at the Windows webview origin, with a
// `window.__TAURI__` whose `shell` command goes to the real shell core and
// whose events a spec emits as the shell would.
import { expect, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { env } from "./helpers";

export const ORIGIN = "http://tauri.localhost";
const DIST = fileURLToPath(new URL("../../apps/desktop/dist/", import.meta.url));

/** The frames each socket of the page sent, in order. */
export type Sockets = string[][];

/** Opens the desktop page as window `label`; resolves once connected (the
 *  quick-ask panel, `#quick-ask`, once its question box shows). */
export async function openDesktop(page: Page, label = "main", hash = ""): Promise<Sockets> {
  // The page's origin is not loopback in Chromium's eyes; the webview's
  // own origin may dial the loopback daemon, as the native app does.
  await page.context().grantPermissions(["local-network-access"], { origin: ORIGIN });
  // The shell installs the annotate agent in every frame of the webview.
  await page.addInitScript({ content: readFileSync(`${DIST}annotate.js`, "utf8") });
  await page.addInitScript((windowLabel: string) => {
    // The shell's bridge exists in the app's own document only.
    if (window.top !== window) return;
    const listeners = new Map<string, ((event: { payload: unknown }) => void)[]>();
    const w = window as unknown as Record<string, unknown>;
    w.__TAURI__ = {
      core: {
        invoke: async (_command: string, args: { request: unknown }) => {
          const res = await fetch(`/__shell?window=${windowLabel}`, { method: "POST", body: JSON.stringify(args.request) });
          const answer = (await res.json()) as { ok?: unknown; err?: string };
          if (answer.err !== undefined) throw answer.err;
          return answer.ok;
        },
      },
      event: {
        listen: async (name: string, handler: (event: { payload: unknown }) => void) => {
          listeners.set(name, [...(listeners.get(name) ?? []), handler]);
          return () => {};
        },
      },
    };
    w.__emit = (name: string, payload: unknown) => {
      for (const handler of listeners.get(name) ?? []) handler({ payload });
    };
  }, label);
  await page.route(`${ORIGIN}/**`, async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/__shell") {
      const res = await fetch(`${env("PMT_DESKTOP_HARNESS")}/shell${url.search}`, { method: "POST", body: route.request().postData() ?? "" });
      return route.fulfill({ status: 200, contentType: "application/json", body: await res.text() });
    }
    const file = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    return route.fulfill({ path: `${DIST}${file}` });
  });
  // Plugin panels: each plugin's own origin, served by the shell core.
  await page.route(/^http:\/\/plugin-[a-z0-9_-]+\.localhost\//, async (route) => {
    const id = new URL(route.request().url()).hostname.replace(/^plugin-/, "").replace(/\.localhost$/, "");
    const res = await fetch(`${env("PMT_DESKTOP_HARNESS")}/plugin-frame?id=${id}`, { method: "POST", body: "" });
    const headers: Record<string, string> = {};
    for (const name of ["content-type", "content-security-policy"]) {
      const value = res.headers.get(name);
      if (value) headers[name] = value;
    }
    return route.fulfill({ status: res.status, headers, body: await res.text() });
  });
  page.on("console", (message) => {
    if (message.type() === "warning" || message.type() === "error") console.log(`[${label}] ${message.text()}`);
  });
  const sockets: Sockets = [];
  page.on("websocket", (ws) => {
    const frames: string[] = [];
    sockets.push(frames);
    ws.on("framesent", (frame) => frames.push(String(frame.payload)));
  });
  await page.goto(`${ORIGIN}/index.html${hash}`);
  if (hash === "#quick-ask") await expect(page.getByLabel("Quick question")).toBeVisible({ timeout: 60_000 });
  else await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 60_000 });
  return sockets;
}

/** The sent requests of `method` across the page's sockets. */
export function sent(sockets: Sockets, method: string): Record<string, unknown>[] {
  return sockets
    .flat()
    .map((f) => JSON.parse(f) as Record<string, unknown>)
    .filter((f) => f.method === method);
}

/** Emits a shell event to the page. */
export async function emit(page: Page, name: string, payload: unknown): Promise<void> {
  await page.evaluate(([n, p]) => (window as unknown as { __emit: (n: string, p: unknown) => void }).__emit(n as string, p), [name, payload]);
}

/** Parses a `prometheus://` link with the shell core, as the app does. */
export async function parseLink(link: string): Promise<unknown> {
  const res = await fetch(`${env("PMT_DESKTOP_HARNESS")}/deeplink`, { method: "POST", body: link });
  return res.json();
}

/** The tickets the shell handed out so far. */
export async function issuedTickets(): Promise<string[]> {
  const res = await fetch(`${env("PMT_DESKTOP_HARNESS")}/tickets`, { method: "POST", body: "" });
  return (await res.json()) as string[];
}

/** Sends `text` from the composer; resolves with the chat's session id. */
export async function say(page: Page, text: string): Promise<string> {
  await page.fill("textarea[name=prompt]", text);
  await page.click("form.composer button[type=submit]");
  const chat = page.locator("section.chat");
  await expect(chat).toHaveAttribute("data-session", /^ses_/);
  return (await chat.getAttribute("data-session")) ?? "";
}

/** What reached the OS through the shell so far. */
export async function nativeCalls(): Promise<string[]> {
  const res = await fetch(`${env("PMT_DESKTOP_HARNESS")}/calls`, { method: "POST", body: "" });
  return (await res.json()) as string[];
}

/** The app was activated (a notification click): what the shell hands
 *  the window, as the Tauri app emits it with `focus-request`. */
export async function activate(page: Page): Promise<void> {
  const res = await fetch(`${env("PMT_DESKTOP_HARNESS")}/activate`, { method: "POST", body: "" });
  const approval = (await res.json()) as unknown;
  expect(approval).not.toBeNull();
  await emit(page, "focus-request", approval);
}
