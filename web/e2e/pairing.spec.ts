// The Pairing page (spec: web-dashboard, Built-in pages; messaging-gateway,
// DM pairing): a stranger's pending request is listed without its code,
// a wrong code is refused, and the code the stranger got in the chat
// (PMT_PAIRING_CODE, from crates/prometheus/tests/dashboard_browser.rs)
// approves them.
import { expect, test } from "@playwright/test";
import { cli, env, signIn } from "./helpers";

test("a pending sender is approved with the code from the chat", async ({ page }) => {
  const code = env("PMT_PAIRING_CODE");
  const url = await signIn(page);
  await page.goto(`${url}pairing`);
  const pending = page.locator('form.approve[data-sender="stranger"]');
  await expect(pending).toHaveCount(1);
  await expect(page.locator(".pairings")).not.toContainText(code);
  await pending.locator("input[name=code]").fill("WRONGCOD");
  await pending.locator("button[name=approve]").click();
  await expect(page.locator(".status .notice.error")).toBeVisible();
  await pending.locator("input[name=code]").fill(code.toLowerCase());
  await pending.locator("button[name=approve]").click();
  await expect(page.locator(".status")).toContainText("Approved stranger on test");
  await expect(page.locator("form.approve")).toHaveCount(0);
  await expect(page.locator(".pairings table")).toContainText("stranger");
  expect(cli("pairing", "list")).toContain("approved");
});
