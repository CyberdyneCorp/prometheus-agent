// Reconnecting (spec: desktop-app, Shared client defaults; Reconnect
// backoff with full jitter; Gone-session verdict) and the backoff policy
// itself.
import { expect, test } from "@playwright/test";
import { Backoff } from "../../apps/desktop/src/backoff";
import { emit, openDesktop, parseLink, say, sent } from "./desktop";
import { cli } from "./helpers";

test("a flapping daemon backs off with full jitter", () => {
  let now = 0;
  const backoff = new Backoff(() => 1, () => now);
  for (let i = 0; i < 3; i++) {
    backoff.opened();
    now += 2_000;
    backoff.closed();
  }
  expect(backoff.attempt).toBe(3);
  expect(backoff.ceiling()).toBe(2_400);
  expect(backoff.delay()).toBe(2_400);
  // A stable connection resets the count; the ceiling is capped at 15 s.
  backoff.opened();
  now += 6_000;
  backoff.closed();
  expect(backoff.attempt).toBe(0);
  for (let i = 0; i < 10; i++) backoff.failed();
  expect(backoff.ceiling()).toBe(15_000);
  expect(new Backoff(() => 0.5).delay()).toBe(150);
});

test("a reconnect re-advertises, re-attaches, and a resume during it keeps the session", async ({ page }) => {
  test.setTimeout(120_000);
  const sockets = await openDesktop(page);
  const id = await say(page, "hello");
  await expect(page.locator(".transcript .message.assistant")).toHaveCount(1, { timeout: 30_000 });

  cli("daemon", "restart");
  await expect.poll(() => sockets.length, { timeout: 30_000 }).toBeGreaterThan(1);
  await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 30_000 });
  const again = JSON.parse(sockets[sockets.length - 1][0]) as { method: string; params: Record<string, unknown> };
  expect(again.method).toBe("rpc.hello");
  expect(again.params).toMatchObject({ client: "desktop", capabilities: { server_requests: true } });
  await expect.poll(() => sent([sockets[sockets.length - 1]], "session.attach").map((f) => (f.params as { session_id: string }).session_id)).toContain(id);

  // The daemon goes away: resuming now keeps the window on the session.
  cli("daemon", "stop");
  await expect(page.locator(".connection")).toHaveText("Reconnecting…", { timeout: 30_000 });
  await page.locator("nav.side").getByRole("button", { name: id }).click();
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", id);
  await expect(page.locator("button[name=retry]")).toBeVisible();
  await expect(page.locator(".chat-title")).toHaveText(`Session ${id}`);

  // Back again: Retry opens it.
  cli("daemon", "start");
  await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 60_000 });
  await page.click("button[name=retry]");
  await expect(page.locator("button[name=retry]")).toHaveCount(0);
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", id);
  await expect(page.locator(".transcript")).toContainText("hello");

  // A session the daemon does not know (-32003) becomes a fresh draft.
  await emit(page, "deep-link", await parseLink("prometheus://session/ses_01J00000000000000000000000"));
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", "");
  await expect(page.locator(".problems")).toContainText("no longer exists");
});
