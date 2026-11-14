// The desktop UI toolset in the window (spec: desktop-app, Desktop UI
// toolset contract; Agent reactions; Terminal pane tools; Preview pane
// tool; Driving the preview page; Annotating the preview page; Reading the
// window below; Pane focus, tours and tips; Appearance tool; Project
// tool), driven by crates/prometheus/tests/desktop_ui.rs: the mock model
// calls each tool and the page answers its `ui.*` request.
import { readFileSync } from "node:fs";
import { expect, type Page, test } from "@playwright/test";
import { openDesktop, say } from "./desktop";
import { env } from "./helpers";

const PREVIEW = "http://preview.test";
const PAGES: Record<string, string> = {
  "/form.html": `<!doctype html><title>Form</title><form><input id="name" placeholder="Name"><input id="pw" type="password"><a id="next" href="/page2.html">Next page</a><button type="button">Send</button></form>`,
  "/page2.html": `<!doctype html><title>Two</title><h1>Page two</h1><button type="button">Back</button>`,
};

const answers = (page: Page) => page.locator(".transcript .message.assistant .text");
const frame = (page: Page) => page.frameLocator("section.preview iframe");

async function said(page: Page, text: string, answer: string | RegExp): Promise<string> {
  const id = await say(page, text);
  await expect(answers(page).last()).toHaveText(answer, { timeout: 60_000 });
  return id;
}

test.describe.configure({ mode: "serial" });

test("the agent opens, marks and drives the preview page", async ({ page }) => {
  test.setTimeout(180_000);
  await page.route(`${PREVIEW}/**`, (route) => {
    const body = PAGES[new URL(route.request().url()).pathname];
    return body ? route.fulfill({ status: 200, contentType: "text/html", body }) : route.fulfill({ status: 404, body: "" });
  });
  await openDesktop(page);
  await said(page, "open the form", "the form is open");
  await expect(frame(page).locator("#pw")).toBeVisible();
  await said(page, "list the elements", "listed");
  await said(page, "mark two fields", "marked");
  await expect(frame(page).locator(".prometheus-mark")).toHaveCount(2);

  // The approval comes before any character is typed.
  await say(page, "type the password");
  const card = page.locator(".card.approval");
  await expect(card).toBeVisible({ timeout: 60_000 });
  await expect(card).toContainText("password field");
  await expect(frame(page).locator("#pw")).toHaveValue("");
  await card.locator('button[data-decision="once"]').click();
  await expect(answers(page).last()).toHaveText("typed", { timeout: 60_000 });
  await expect(frame(page).locator("#pw")).toHaveValue("hunter2");

  await said(page, "follow the link", "on page two");
  await expect(frame(page).locator("h1")).toHaveText("Page two");
  await expect(frame(page).locator(".prometheus-mark")).toHaveCount(0);
  await said(page, "click the old ref", "asked for new refs");
  // A tour step on the preview surface is a labelled mark in the page.
  await said(page, "point at the heading", "pointed");
  await expect(frame(page).locator(".prometheus-mark")).toHaveCount(1);
  await expect(frame(page).locator(".prometheus-mark")).toContainText("This page");
});

test("a background process gets a mirror tab that closes without stopping it", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  await said(page, "start the server", "ok");
  await page.selectOption("select[name=layout]", "quad");
  const mirror = page.locator("pre.terminal-screen.mirror");
  await expect(mirror).toContainText("serving on 5173", { timeout: 30_000 });
  await said(page, "read the terminal", "the server is serving");
  await said(page, "close the mirror", "ok");
  await expect(mirror).toHaveCount(0);
  // A closed mirror stays closed while the process runs on.
  await page.waitForTimeout(2500);
  await expect(mirror).toHaveCount(0);
  await said(page, "is the server alive", "still running");
});

test("the window below reports an untitled macOS window with the permission it needs", async ({ page }) => {
  await openDesktop(page, "main", "", env("PMT_DESKTOP_HARNESS_MAC"));
  await said(page, "what is below", "Safari is below; its title needs Screen Recording");
});

test("appearance, panes, tips and tours", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  const accent = () => page.evaluate(() => document.documentElement.style.getPropertyValue("--accent"));
  const before = await accent();
  // An unknown layout changes nothing, not even the accent named with it.
  await said(page, "use the cockpit layout", "no such layout");
  expect(await accent()).toBe(before);
  await said(page, "go violet and dark", "ok");
  await expect.poll(accent).toBe("#7048e8");
  await expect(page.locator("html")).toHaveAttribute("data-mode", "dark");
  await said(page, "show me the files", "ok");
  await expect(page.locator('.pane[data-pane="files"]')).toBeVisible();
  await said(page, "give me a tip", "ok");
  await expect(page.locator(".tip.bubble")).toContainText("Type your question here");
  await say(page, "take the tour");
  const bubble = page.locator(".tour.bubble");
  await expect(bubble).toContainText("1 / 3", { timeout: 60_000 });
  for (let i = 0; i < 3; i++) await bubble.locator("button[name=tour-next]").click();
  await expect(bubble).toHaveCount(0);
  await expect(answers(page).last()).toHaveText("tour done", { timeout: 60_000 });
});

test("switching project shows the chat under its new project", async ({ page }) => {
  await openDesktop(page);
  await page.locator('section.projects button[name=open-project][data-project="api"]').click();
  const id = await said(page, "work on web", "switched to web");
  await expect(page.locator(`section.projects li[data-project="web"] li[data-session="${id}"]`)).toBeVisible();
  await expect(page.locator(`section.projects li[data-project="api"] li[data-session="${id}"]`)).toHaveCount(0);
});

test("with reactions on, the agent reacts to an earlier message", async ({ page }) => {
  await openDesktop(page);
  await page.locator("input[name=message_reactions]").check();
  await expect.poll(() => readFileSync(`${env("PMT_HOME")}/ui-prefs.json`, "utf8")).toMatch(/"message_reactions":\s*true/);
  await said(page, "hello there", "ok");
  await said(page, "react to my first message", "ok");
  await expect(page.locator('.message.user[data-prompt="1"] .reaction')).toHaveText("😂");
  await expect(page.locator('.message.user[data-prompt="2"] .reaction')).toHaveCount(0);
});
