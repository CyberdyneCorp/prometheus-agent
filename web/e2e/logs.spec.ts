// Logs page is redacted and bounded (web-dashboard): the page tails a log
// of the registry; a path is refused.
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("the logs page tails registry logs and refuses paths", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}logs?file=gateway`);
  await expect(page.locator('select[name="file"]')).toHaveValue("gateway");
  await expect(page.locator("table, .notice").first()).toBeVisible();
  const traversal = await page.request.get(`${url}api/logs?file=../../secrets.toml`);
  expect(traversal.status()).toBe(400);
  expect((await traversal.json()).message).toBe("unknown log file");
});
