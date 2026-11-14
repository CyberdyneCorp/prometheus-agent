// Background running (spec: desktop-app): Background approval (Approval
// notifications never approve), Quit during a turn (Closing windows does
// not stop the agent), Plugin reads another plugin's storage and
// Ungranted capability (Plugin UI contributions are isolated), and Quick
// follow-up (Global hotkey quick-ask, through the quick-ask panel).
import { expect, test } from "@playwright/test";
import { activate, emit, nativeCalls, openDesktop, say, sent } from "./desktop";

test("a notification click opens the approval card without approving", async ({ page }) => {
  const sockets = await openDesktop(page);
  // The window is minimised: the shell reports it unfocused.
  await emit(page, "window-focus", { focused: false });
  await say(page, "clean the build");
  const card = page.getByRole("article", { name: "Approval request" });
  await expect(card).toBeVisible();
  await expect.poll(nativeCalls).toContain("notify Approval needed: terminal");
  await page.locator(".transcript").evaluate((el) => (el.scrollTop = 0));
  // The user clicks the notification: the OS activates the app.
  await emit(page, "window-focus", { focused: true });
  await activate(page);
  await expect(card).toHaveAttribute("data-focused", "true");
  await expect(card).toBeFocused();
  // Nothing was answered: the request stays pending for the card.
  await expect(card.getByRole("button", { name: "Deny" })).toBeEnabled();
  await expect(card.getByRole("status")).toHaveText("");
  expect(sent(sockets, "approval.respond")).toEqual([]);
  await card.getByRole("button", { name: "Deny" }).click();
  await expect(card.getByRole("status")).toHaveText("Denied");
  expect(sent(sockets, "approval.respond")).toHaveLength(1);
});

test("quitting during a turn leaves it running in the daemon", async ({ browser }) => {
  const first = await browser.newContext();
  const window = await first.newPage();
  await openDesktop(window);
  const runningLine = "tray Pending approvals (0) | Running sessions (1)";
  const running = async () => (await nativeCalls()).filter((c) => c === runningLine).length;
  const before = await running();
  const session = await say(window, "slow job");
  // The turn runs (the tray lists it), then the user quits: the window and
  // its connection go away mid-turn.
  await expect.poll(running).toBe(before + 1);
  await expect(window.locator(".message.assistant")).toHaveCount(0);
  await first.close();
  const next = await browser.newContext();
  const again = await next.newPage();
  await openDesktop(again);
  await again.locator(`button[data-session="${session}"]`).click();
  await expect(again.locator(".message.assistant").last()).toContainText("finished slowly", { timeout: 30_000 });
  await next.close();
});

test("plugin panels keep their storage and capabilities to themselves", async ({ page }) => {
  await openDesktop(page);
  const a = page.frameLocator('iframe[data-plugin="a"]');
  const b = page.frameLocator('iframe[data-plugin="b"]');
  await expect(a.locator("#keys")).toHaveText("own-a");
  await expect(b.locator("#keys")).toHaveText("own-b");
  await expect(a.locator("#sessions")).toHaveText("capability sessions.read not granted to plugin a");
  await expect(b.locator("#sessions")).toHaveText(/^sessions \d+$/);
  await expect(a.locator("#native")).toHaveText("read_clipboard is not available to plugins");
  await expect(page.getByText(/Plugin c has a panel that does not load/)).toBeVisible();
  await expect.poll(nativeCalls).not.toContain("read_clipboard");
});

test("a quick follow-up goes to the same quick-ask session", async ({ page }) => {
  const sockets = await openDesktop(page, "quick-ask", "#quick-ask");
  const box = page.getByLabel("Quick question");
  await box.fill("hello");
  await box.press("Enter");
  await expect(page.locator(".message.assistant").last()).toContainText("hi there");
  await box.fill("hello again");
  await box.press("Enter");
  await expect(page.locator(".message.assistant")).toHaveCount(2);
  expect(sent(sockets, "session.create")).toHaveLength(1);
  const asked = sent(sockets, "prompt.submit").map((f) => (f.params as { session_id: string }).session_id);
  expect(asked).toHaveLength(2);
  expect(asked[1]).toBe(asked[0]);
});
