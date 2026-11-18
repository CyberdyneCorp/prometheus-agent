// The desktop's bot screen pane (spec: execution-environments, Bot
// screen; Bot screen takeover lease), driven by crates/prometheus/tests/
// desktop_screen.rs against the real daemon and a stand-in Xvnc that
// serves a red 120x100 framebuffer over RFB and records the input it gets.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { openDesktop } from "./desktop";
import { env } from "./helpers";

/** The input events the screen received. */
function inputs(): Record<string, unknown>[] {
  const record = JSON.parse(readFileSync(join(env("PMT_HOME"), "profiles/default/run/screen/screen.json"), "utf8")) as { rfb: string };
  const log = `${record.rfb}.input`;
  if (!existsSync(log)) return [];
  return readFileSync(log, "utf8")
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line) as Record<string, unknown>);
}

/** Who the CLI says holds the screen. */
function holder(): unknown {
  const home = env("PMT_USER_HOME");
  const out = spawnSync(env("PMT_BIN"), ["--home", env("PMT_HOME"), "--json", "computer-use", "screen", "status"], { encoding: "utf8", env: { ...process.env, HOME: home } });
  return (JSON.parse(out.stdout) as { holder: unknown }).holder;
}

test("the screen pane streams the bot screen and takes it over", async ({ page }) => {
  await openDesktop(page);
  await page.click('button[name=pane][data-pane="screen"]');
  const pane = page.locator("section.screen-pane");
  const canvas = page.locator(".screen-canvas");
  await expect(pane).toHaveAttribute("data-stream", "open");
  await expect(canvas).toHaveAttribute("data-size", "120x100");
  await expect
    .poll(() => canvas.evaluate((c: HTMLCanvasElement) => Array.from(c.getContext("2d")?.getImageData(60, 50, 1, 1).data ?? [])))
    .toEqual([255, 0, 0, 255]);
  await expect(pane).toHaveAttribute("data-control", "agent");
  await expect(page.locator(".screen-status")).toContainText("The agent has control");

  // Read-only while the agent has control.
  await canvas.click({ position: { x: 10, y: 10 } });
  await page.keyboard.press("a");
  await page.waitForTimeout(500);
  expect(inputs()).toEqual([]);

  // Taken over: the agent waits, and this window's input reaches the screen.
  await page.click("button[name=screen-take-over]");
  await expect(pane).toHaveAttribute("data-control", "you");
  await expect(page.locator(".screen-status")).toContainText("You have control");
  expect(holder()).toBe("desktop");
  await canvas.click({ position: { x: 10, y: 10 } });
  await page.keyboard.press("a");
  await expect.poll(() => inputs()).toContainEqual({ key: 0x61, down: true });
  expect(inputs().some((e) => Array.isArray(e.pointer) && e.buttons === 1)).toBe(true);

  // The browser launcher: on the screen's display, with the profile the
  // agent's headed browser uses there.
  await page.click("button[name=screen-browser]");
  await expect(page.locator(".screen-status")).toContainText("Opened a browser");
  const launched = join(env("PMT_USER_HOME"), "fake-chromium.log");
  await expect.poll(() => existsSync(launched)).toBe(true);
  const [display, ...args] = readFileSync(launched, "utf8").trim().split(" ");
  const record = JSON.parse(readFileSync(join(env("PMT_HOME"), "profiles/default/run/screen/screen.json"), "utf8")) as { display: string };
  expect(display).toBe(record.display);
  expect(args).toContain(`--user-data-dir=${join(env("PMT_HOME"), "profiles/default/cache/browser/screen-profile")}`);
  expect(args).toContain("about:blank");

  // A new connection (a reopened pane) learns who has control from its ticket.
  await page.reload();
  await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 60_000 });
  await page.click('button[name=pane][data-pane="screen"]');
  await expect(pane).toHaveAttribute("data-control", "you");

  // Handed back: the agent has it again.
  await page.click("button[name=screen-hand-back]");
  await expect(pane).toHaveAttribute("data-control", "agent");
  expect(holder()).toBeNull();
});
