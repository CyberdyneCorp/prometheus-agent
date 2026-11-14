// The desktop's workspace panes (spec: desktop-app, File browser pane;
// Artifacts gallery and suggestions; Preview rail and in-app browser;
// Annotate mode in the in-app browser), driven by crates/prometheus/tests/
// desktop_panes.rs against the real daemon with a scripted mock model.
import { existsSync } from "node:fs";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { nativeCalls, openDesktop, say, sent } from "./desktop";
import { env } from "./helpers";

const PREVIEW = "http://preview.test";
const answers = (page: Page) => page.locator(".transcript .message.assistant");

const PAGES: Record<string, string> = {
  "/form.html": `<!doctype html><title>form</title><body><input id="name"><script>parent.postMessage({loaded: true}, "*")</script></body>`,
  "/native.html": `<!doctype html><title>native</title><body><p id="result">pending</p><script>
    (async () => {
      try {
        await window.__TAURI__.core.invoke("shell", { request: { command: "reveal_in_folder", args: { path: "/etc/hosts" } } });
        document.getElementById("result").textContent = "reached the shell";
      } catch (err) {
        document.getElementById("result").textContent = "failed: " + err;
      }
    })();
  </script></body>`,
  "/login.html": `<!doctype html><title>login</title><body><header><h1>Sign in</h1></header><main>
    <form id="login" data-api-token="sk-live-abcdefghijklmnopqrstuvwxyz123456"><input name="user" value="bob"><input type="password" name="pw"><input type="hidden" name="csrf" value="c5f1e"><button id="go" type="button">Go</button></form>
    <p id="a">one</p><p id="b">two</p><p id="c">three</p></main></body>`,
};

async function servePreviews(page: Page): Promise<void> {
  await page.route(`${PREVIEW}/**`, (route) => {
    const body = PAGES[new URL(route.request().url()).pathname];
    return body ? route.fulfill({ status: 200, contentType: "text/html", body }) : route.fulfill({ status: 404, body: "" });
  });
}

async function openPreview(page: Page, path: string): Promise<void> {
  await page.click('button[name=pane][data-pane="preview"]');
  await page.fill("input[name=preview-url]", `${PREVIEW}${path}`);
  await page.click("button[name=preview-go]");
}

const frame = (page: Page) => page.frameLocator("section.preview iframe");

test("a large file previews its first 2 MiB and opens through the guarded opener", async ({ page }) => {
  await openDesktop(page);
  await say(page, "hello");
  await page.click('button[name=pane][data-pane="files"]');
  await page.click('button.file[data-path="big.log"]');
  await expect(page.locator(".file-preview")).toContainText("Showing the first 2.0 MiB of 40.0 MiB.");
  const shown = await page.locator(".file-preview pre").evaluate((el) => (el.textContent ?? "").length);
  expect(shown).toBe(2 * 1024 * 1024);
  await page.click("button[name=open-externally]");
  await expect.poll(async () => (await nativeCalls()).filter((c) => c.startsWith("open_path") && c.endsWith("big.log"))).toHaveLength(1);
  // Entries .gitignore ignores show only on request.
  await expect(page.locator('[data-path="ignored.tmp"]')).toHaveCount(0);
  await page.check("input[name=show-ignored]");
  await expect(page.locator('button[data-path="ignored.tmp"]')).toHaveCount(1);
});

test("a dismissed suggestion stays dismissed until the file is written again", async ({ page }) => {
  await openDesktop(page);
  const id = await say(page, "write the report");
  await expect(answers(page).last()).toHaveText(/Written/);
  const suggestion = page.locator('.suggestions .suggestion[data-path="report.html"]');
  await expect(suggestion).toHaveCount(1);
  await suggestion.locator("button[name=dismiss-suggestion]").click();
  await page.reload();
  await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 60_000 });
  await page.click(`nav.side [data-session="${id}"]`);
  await expect(page.locator(".chat-title")).toHaveText(`Session ${id}`);
  await page.waitForTimeout(1_000);
  await expect(suggestion).toHaveCount(0);
  expect(existsSync(join(env("PMT_HOME"), "profiles/default/workspace/report.html"))).toBe(true);
  // The artifacts view lists it with its session.
  await page.click('button[name=pane][data-pane="artifacts"]');
  await expect(page.locator(`.artifact[data-value="report.html"] button[name=jump][data-session="${id}"]`).first()).toBeVisible();
  await page.fill("textarea[name=prompt]", "rewrite it again");
  await page.click("form.composer button[type=submit]");
  await expect(answers(page).last()).toHaveText(/Rewritten/);
  await expect(suggestion).toHaveCount(1);
});

test("a hidden page keeps its form state and a page has no native command channel", async ({ page }) => {
  await servePreviews(page);
  await openDesktop(page);
  await openPreview(page, "/form.html");
  await frame(page).locator("#name").fill("half filled");
  const loads = await page.locator("section.preview iframe").getAttribute("data-loads");
  await page.click("button[name=preview-hide]");
  await expect(page.locator("section.preview")).toBeHidden();
  await page.click('button[name=pane][data-pane="preview"]');
  await expect(frame(page).locator("#name")).toHaveValue("half filled");
  expect(await page.locator("section.preview iframe").getAttribute("data-loads")).toBe(loads);
  await openPreview(page, "/native.html");
  await expect(frame(page).locator("#result")).toContainText("failed");
  expect((await nativeCalls()).filter((c) => c.startsWith("reveal"))).toEqual([]);
  // Only http, https and workspace files load.
  await page.fill("input[name=preview-url]", "javascript:alert(1)");
  await page.click("button[name=preview-go]");
  await expect(page.locator(".preview-status")).toContainText("do not open in the preview");
});

test("annotations redact secrets and pins never send", async ({ page }) => {
  await servePreviews(page);
  const sockets = await openDesktop(page);
  await say(page, "hello");
  await expect(answers(page)).toHaveCount(1);
  const submitted = sent(sockets, "prompt.submit").length;
  await openPreview(page, "/login.html");
  await frame(page).locator("input[name=pw]").fill("hunter2-secret");
  await page.click("button[name=annotate]");
  const pin = async (selector: string, note: string) => {
    await frame(page).locator(selector).click();
    await page.fill("input[name=pin-note]", note);
    await page.click("button[name=save-pin]");
  };
  await pin("#login", "the login form");
  await pin("#a", "first");
  await pin("#b", "second");
  await expect(page.locator(".pins .pin")).toHaveCount(3);
  await page.locator('.pin[data-pin="2"] button[name=delete-pin]').click();
  expect(await page.locator(".pins .pin").evaluateAll((els) => els.map((e) => e.getAttribute("data-pin")))).toEqual(["1", "3"]);
  await page.waitForTimeout(500);
  expect(sent(sockets, "prompt.submit")).toHaveLength(submitted);
  await page.locator('.pin[data-pin="3"] button[name=delete-pin]').click();
  await page.click("button[name=add-comments]");
  const text = await page.locator("textarea[name=prompt]").inputValue();
  expect(text).toContain("Comment 1: the login form");
  expect(text).toContain('type="password"');
  expect(text).toContain("[redacted]");
  expect(text).not.toContain("hunter2");
  expect(text).not.toContain("sk-live");
  expect(text).not.toContain("c5f1e");
  expect(text).toMatch(/Styles: display: /);
  // The pin's snapshot (of the redacted element) is staged as an image.
  await expect(page.locator("ul.staged li")).toHaveText(["snapshot-1.png×"]);
  expect(sent(sockets, "prompt.submit")).toHaveLength(submitted);
});
