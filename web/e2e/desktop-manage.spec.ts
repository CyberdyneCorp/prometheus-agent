// The management panes, the messaging pane with Telegram quick setup, the
// Projects sidebar's discovery, shortcuts, the palette and profile files
// (spec: desktop-app, Management panes; Messaging pane shows saved versus
// running state; Telegram quick setup by QR; Repository discovery for the
// Projects sidebar; Fonts, shortcuts, palette and profile files), driven
// by crates/prometheus/tests/desktop_manage.rs, which passes the Telegram
// onboarding stand-in's URL in PMT_TELEGRAM_SETUP.
import { expect, type Page, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { openDesktop, sent } from "./desktop";
import { cli, env } from "./helpers";

async function manage(page: Page, sub: string): Promise<void> {
  await page.click('button[name=pane][data-pane="manage"]');
  await page.click(`button[name=manage-page][data-page="${sub}"]`);
  await expect(page.locator(".manage-page")).toHaveAttribute("data-page", sub);
  await expect(page.locator(".manage-page")).toHaveAttribute("data-ready", "true");
}

test("pausing another profile's job targets its owning profile", async ({ page }) => {
  const sockets = await openDesktop(page);
  await page.selectOption("select[name=profile]", "work");
  await expect(page.locator(".connection")).toHaveText(/Connected · work/, { timeout: 30_000 });
  await manage(page, "jobs");
  await page.check("input[name=all-profiles]");
  const job = page.locator('li[data-owner="home"]');
  await expect(job).toHaveCount(1);
  await job.locator("button[name=pause-job]").click();
  await expect(page.locator('li[data-owner="home"]')).toHaveAttribute("data-state", "paused");
  const pause = sent(sockets, "manage.change").find((f) => (f.params as { change: string }).change === "cron_update");
  expect((pause?.params as { profile: string }).profile).toBe("home");
  // `Applies to` shows with several profiles and follows the window's profile.
  await expect(page.locator("select[name=manage-applies-to]")).toHaveValue("work");
});

test("a saved token of a disabled adapter shows saved, and a toggle asks for a restart", async ({ page }) => {
  await openDesktop(page);
  await manage(page, "messaging");
  const slack = page.locator('article.adapter[data-platform="slack"]');
  await expect(slack.locator(".adapter-state")).toHaveAttribute("data-state", "saved");
  await expect(page.locator('article.adapter[data-platform="discord"] .adapter-state')).toHaveAttribute("data-state", "not_configured");
  await slack.locator("input[name=adapter-enabled]").check();
  const enabled = page.locator('article.adapter[data-platform="slack"]');
  await expect(enabled.locator(".restart-banner")).toBeVisible();
  await expect(enabled.locator(".adapter-state")).toHaveAttribute("data-state", "pending_restart");
});

test("closing the QR dialog while polling discards the late result and writes nothing", async ({ page }) => {
  test.setTimeout(90_000);
  const sockets = await openDesktop(page);
  const config = `${env("PMT_HOME")}/profiles/default/config.toml`;
  const before = readFileSync(config, "utf8");
  await manage(page, "messaging");
  const telegram = page.locator('article.adapter[data-platform="telegram"] .adapter-state');
  const state = await telegram.getAttribute("data-state");
  await page.locator('article.adapter[data-platform="telegram"] button[name=create-with-qr]').click();
  // The QR shows once the pairing exists; the stand-in holds the first poll.
  await expect(page.locator(".qr-dialog .qr-step img.qr")).toBeVisible({ timeout: 30_000 });
  await page.click("button[name=close-qr]");
  await expect(page.locator(".qr-dialog")).toHaveCount(0);
  await expect.poll(() => sent(sockets, "adapters.pairing.cancel").length).toBe(1);
  // Let the daemon handle the cancel before the held poll answers.
  await page.waitForTimeout(1_000);
  const released = (await (await fetch(`${env("PMT_TELEGRAM_SETUP")}/release`, { method: "POST" })).json()) as { delivered: boolean };
  expect(released.delivered).toBe(true);
  // The late "ready" reached the daemon; give it time to (not) act on it.
  await page.waitForTimeout(2_000);
  expect(cli("--json", "secrets", "list")).not.toContain("TELEGRAM_BOT_TOKEN");
  expect(readFileSync(config, "utf8")).toBe(before);
  await manage(page, "messaging");
  await expect(telegram).toHaveAttribute("data-state", state ?? "");
  await expect(page.locator('article.adapter[data-platform="telegram"] input[name=TELEGRAM_BOT_TOKEN]')).toHaveAttribute("placeholder", "not set");
});

test("Create with QR stores the token, allowlists the owner and connects Telegram", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  await manage(page, "messaging");
  await page.locator('article.adapter[data-platform="telegram"] button[name=create-with-qr]').click();
  const step = page.locator(".qr-dialog .qr-step");
  await expect(step.locator("img.qr")).toBeVisible({ timeout: 30_000 });
  await expect(step.locator("a.deep-link")).toHaveText("https://t.me/newbot/SetupBot/my_prometheus_bot");
  await expect(step).toHaveAttribute("data-state", "linked", { timeout: 60_000 });
  await page.click("button[name=close-qr]");
  await expect(async () => {
    await manage(page, "messaging");
    await expect(page.locator('article.adapter[data-platform="telegram"] .adapter-state')).toHaveAttribute("data-state", "connected", { timeout: 2_000 });
  }).toPass({ timeout: 60_000 });
});

test("turning scanning off removes discovered rows and keeps explicit projects", async ({ page }) => {
  await openDesktop(page);
  const sidebar = page.locator("section.projects");
  await expect(sidebar.locator('li[data-kind="discovered"]')).toHaveCount(2, { timeout: 30_000 });
  await expect(sidebar.locator('li[data-project="gamma"]')).toBeVisible();
  await expect(sidebar.locator('li[data-kind="discovered"][data-repo$="/gamma"]')).toHaveCount(0);
  await sidebar.locator("input[name=repo-scan]").uncheck();
  await expect(sidebar.locator('li[data-kind="discovered"]')).toHaveCount(0);
  await expect(sidebar.locator('li[data-project="gamma"]')).toBeVisible();
});

test("a duplicate chord flags both shortcuts until one changes", async ({ page }) => {
  await openDesktop(page);
  await manage(page, "shortcuts");
  const newSession = page.locator('input[data-shortcut="new-session"]');
  await newSession.fill("CommandOrControl+G");
  await newSession.press("Enter");
  await newSession.blur();
  await expect(page.locator('li[data-action="new-session"]')).toHaveAttribute("data-conflict", "true");
  await expect(page.locator('li[data-action="review"]')).toHaveAttribute("data-conflict", "true");
  await expect(page.locator('li[data-action="review"] .conflict')).toContainText("New session");
  await page.locator('input[data-shortcut="new-session"]').fill("CommandOrControl+Shift+Y");
  await page.locator('input[data-shortcut="new-session"]').blur();
  await expect(page.locator("li[data-conflict=true]")).toHaveCount(0);
  // The machine keeps the binding.
  await page.reload();
  await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 60_000 });
  await manage(page, "shortcuts");
  await expect(page.locator('input[data-shortcut="new-session"]')).toHaveValue("CommandOrControl+Shift+Y");
});

test("the palette finds pages, sessions and actions and exports a profile without keys", async ({ page }) => {
  await openDesktop(page);
  await page.keyboard.press("Control+k");
  const palette = page.locator(".command-palette");
  await expect(palette).toBeVisible();
  await page.fill("input[name=palette-query]", "page review");
  await page.keyboard.press("Enter");
  await expect(page.locator('.pane[data-pane="review"]')).toBeVisible();
  await page.keyboard.press("Control+p");
  await page.fill("input[name=palette-query]", "Export profile work");
  await expect(palette.locator("li")).toHaveCount(1);
  await page.keyboard.press("Enter");
  await expect(page.locator(".notices")).toContainText("Exported profile work", { timeout: 30_000 });
  // desktop.font_family goes in front of the theme's font stack.
  await expect.poll(() => page.evaluate(() => getComputedStyle(document.body).fontFamily)).toContain("Fira Sans");
  // desktop.terminal_font_family is empty: the terminal uses the bundled font.
  const terminal = await page.evaluate(async () => {
    const probe = document.createElement("pre");
    probe.className = "terminal-screen";
    document.body.append(probe);
    const family = getComputedStyle(probe).fontFamily;
    probe.remove();
    const faces = await document.fonts.load('16px "JetBrains Mono"', "prometheus");
    return { family, loaded: faces.filter((f) => f.status === "loaded").length };
  });
  expect(terminal.family).toMatch(/^"JetBrains Mono"/);
  expect(terminal.loaded).toBeGreaterThan(0);
  // Zoom goes in half steps.
  await page.keyboard.press("Control+=");
  await expect(page.locator("html")).toHaveAttribute("data-zoom", "0.5");
  await page.keyboard.press("Control+0");
  await expect(page.locator("html")).toHaveAttribute("data-zoom", "0");
});
