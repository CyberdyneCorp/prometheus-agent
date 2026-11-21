// Scenario: First scan does not block (pets, Achievements from session
// history). The profile was never scanned and holds 400 sessions: the page
// renders at once with a scanning indicator and fills in when the
// background scan finishes. crates/prometheus/tests/pets_e2e.rs seeds the
// history.
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("the page shows a scanning indicator, then the badges", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}achievements`);
  await expect(page.locator("section.page h1")).toHaveText("Achievements");
  const status = page.locator(".scan-state");
  await expect(status).toHaveAttribute("data-scanning", "true");
  await expect(status).toContainText("Scanning your session history");
  await expect(status).toHaveAttribute("data-scanning", "false", { timeout: 60_000 });
  await expect(status).toContainText("400 sessions analysed");
  const regular = page.locator('article.badge[data-id="regular"]');
  await expect(regular).toHaveAttribute("data-tier", "silver");
  await expect(regular.locator(".what")).toHaveText("Sessions in all.");
  await expect(regular.locator(".count")).toHaveText("400 / 500 for gold");
  await expect(page.locator('article.badge[data-id="night-owl"]')).toBeVisible();
});
