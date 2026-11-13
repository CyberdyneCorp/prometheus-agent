// Shared steps of the dashboard specs: a fresh one-time login link from
// the CLI (as a user would get it), signing a page in, and the CSRF token
// a raw call needs.
import { execFileSync } from "node:child_process";
import { expect, type Page } from "@playwright/test";

export interface Opened {
  url: string;
  login_url: string;
}

export function env(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set: run this spec through crates/prometheus/tests/dashboard_e2e.rs`);
  return value;
}

/** `prometheus dashboard --no-open --json` on the harness's home (any
 *  free port when it starts the listener; the running one otherwise),
 *  with the harness's isolated user home. */
export function dashboard(): Opened {
  const args = ["--home", env("PMT_HOME"), "--json", "dashboard", "--no-open", "--port", "0"];
  const home = env("PMT_USER_HOME");
  const child = { ...process.env, HOME: home, USERPROFILE: home };
  return JSON.parse(execFileSync(env("PMT_BIN"), args, { encoding: "utf8", env: child })) as Opened;
}

/** Runs the CLI on the harness's home with its isolated user home. */
export function cli(...args: string[]): string {
  const home = env("PMT_USER_HOME");
  const child = { ...process.env, HOME: home, USERPROFILE: home };
  return execFileSync(env("PMT_BIN"), ["--home", env("PMT_HOME"), ...args], { encoding: "utf8", env: child });
}

/** Signs `page` in with a new login link; the dashboard's URL. */
export async function signIn(page: Page): Promise<string> {
  const opened = dashboard();
  await page.goto(opened.login_url);
  await expect(page.locator("header.top")).toBeVisible();
  return opened.url;
}

/** The signed-in page's CSRF token. */
export async function csrf(page: Page, url: string): Promise<string> {
  const response = await page.request.get(`${url}api/session`);
  expect(response.status()).toBe(200);
  return ((await response.json()) as { csrf: string }).csrf;
}
