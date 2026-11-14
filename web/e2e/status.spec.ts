// Scenario: Status page exists (web-dashboard, Built-in pages; Honest
// platform support matrix).
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("the status page shows the daemon and the platform matrix", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}status`);
  await expect(page.getByRole("heading", { name: "Status" })).toBeVisible();
  await expect(page.locator('[data-fact="version"]')).toContainText(/\d+\.\d+\.\d+/);
  await expect(page.locator('[data-fact="uptime"]')).toContainText(/\d+h \d+m/);
  await expect(page.locator('[data-fact="clients"]')).toHaveText(/^\d+$/);
  await expect(page.locator('[data-fact="sessions"]')).toHaveText(/^\d+$/);
  await expect(page.getByRole("heading", { name: "Adapters" })).toBeVisible();
  await expect(page.locator('[data-empty="adapters"]')).toBeVisible();
  const sandbox = page.locator('[data-feature="sandbox.local"]');
  await expect(sandbox).toHaveText(/^(supported|degraded|unsupported)$/);
  const reason = sandbox.locator("xpath=../following-sibling::td[1]");
  await expect(reason).not.toBeEmpty();
});
