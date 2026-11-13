// Windows, tabs and panes, interface modes, terminals and the live
// subagents frame (spec: desktop-app, Windows, tabs and panes; Interface
// modes and layouts; Embedded terminals are daemon-owned PTYs; Live
// subagents frame), driven by crates/prometheus/tests/desktop_workspace.rs.
import { expect, type Page, test } from "@playwright/test";
import { nativeCalls, openDesktop, say } from "./desktop";

const answers = (page: Page) => page.locator(".transcript .message.assistant");

test("a popped-out session streams the turn with the same seq as its window", async ({ browser }) => {
  test.setTimeout(120_000);
  const one = await (await browser.newContext()).newPage();
  await openDesktop(one, "one");
  const id = await say(one, "count slowly");
  await one.click("button[name=pop-out]");
  await expect.poll(async () => await nativeCalls()).toContain(`window #session=default/${id}`);
  const two = await (await browser.newContext()).newPage();
  await openDesktop(two, "two", `#session=default/${id}`);
  await expect(two.locator("section.chat")).toHaveAttribute("data-session", id);
  await expect(two.locator("nav.side")).toHaveCount(0);
  await expect(answers(one).last()).toHaveText(/counted/, { timeout: 60_000 });
  await expect(answers(two).last()).toHaveText(/counted/, { timeout: 60_000 });
  const seq = await one.locator("section.chat").getAttribute("data-last-seq");
  expect(Number(seq)).toBeGreaterThan(0);
  await expect(two.locator("section.chat")).toHaveAttribute("data-last-seq", seq ?? "");
});

test("tabs open, cycle, close and reopen", async ({ page }) => {
  await openDesktop(page);
  const id = await say(page, "first tab");
  await page.keyboard.press("Control+t");
  await expect(page.locator("nav.chat-tabs button[name=tab]")).toHaveCount(2);
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", "");
  await page.keyboard.press("Control+Tab");
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", id);
  await page.keyboard.press("Control+w");
  await expect(page.locator("nav.chat-tabs button[name=tab]")).toHaveCount(1);
  await page.keyboard.press("Control+Shift+T");
  await expect(page.locator("nav.chat-tabs button[name=tab]")).toHaveCount(2);
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", id);
});

test("switching back to advanced restores its own arrangement", async ({ page }) => {
  await openDesktop(page);
  await page.click('button[name=pane][data-pane="files"]');
  await page.keyboard.press("Control+b");
  await expect(page.locator("nav.side")).toBeHidden();
  await page.selectOption("select[name=interface-mode]", "simple");
  await expect(page.locator("nav.side")).toBeVisible();
  await expect(page.locator("footer.status-bar")).toBeHidden();
  // Shortcuts keep working in simple mode.
  await page.keyboard.press("Control+b");
  await expect(page.locator("nav.side")).toBeHidden();
  await page.selectOption("select[name=interface-mode]", "advanced");
  await expect(page.locator("nav.side")).toBeHidden();
  await expect(page.locator('.pane[data-pane="files"]')).toBeVisible();
  await expect(page.locator("footer.status-bar")).toBeVisible();
  // A preset opens what it places and closes the rest.
  await page.selectOption("select[name=layout]", "terminal-deck");
  await expect(page.locator("section.terminals")).toBeVisible();
  await expect(page.locator("nav.side")).toBeHidden();
});

test("a terminal is a shell of the session's workspace", async ({ page }) => {
  await openDesktop(page);
  await say(page, "hello");
  await page.selectOption("select[name=layout]", "quad");
  await page.click("button[name=new-terminal]");
  const screen = page.locator("pre.terminal-screen");
  await expect(screen).toBeFocused();
  await page.keyboard.type("echo e2e-$((40+2))");
  await page.keyboard.press("Enter");
  await expect(screen).toContainText("e2e-42");
  // CommandOrControl+W over the terminal does not close the tab.
  await page.keyboard.press("Control+w");
  await expect(page.locator("nav.chat-tabs button[name=tab]")).toHaveCount(1);
});

test("steering a running subagent is acknowledged as queued", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  await say(page, "delegate the slow job");
  const frame = page.locator("section.subagents-frame");
  await expect(frame).toContainText("1 subagent running", { timeout: 60_000 });
  await frame.locator("button[name=subagent]").first().click();
  await page.fill("input[name=steer-text]", "focus on the tests");
  await page.click("button[name=steer]");
  await expect(page.locator(".steer-result")).toHaveText("guidance queued for next checkpoint");
  await page.click("button[name=stop-subagent]");
  await expect(page.locator(".steer-result")).toHaveText("stop requested");
});
