// Scenario: Excluded path skipped (projects, Repository discovery), on the
// dashboard: the scan's discovered repositories are offered as one-click
// project creation, an excluded subtree is not listed, and creating a
// project moves the repository to the explicit projects. Driven by
// crates/prometheus/tests/dashboard_browser.rs.
import { expect, test } from "@playwright/test";
import { cli, signIn } from "./helpers";

test("discovered repositories become projects in one click; excluded ones are not offered", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}projects`);
  const view = page.locator(".projects-page");
  const discovered = view.locator('li[data-kind="discovered"]');
  await expect(discovered).toHaveCount(2, { timeout: 30_000 });
  await expect(view.locator('li[data-kind="discovered"][data-repo$="/alpha"]')).toHaveCount(1);
  await expect(view.locator('li[data-kind="discovered"][data-repo$="/beta"]')).toHaveCount(1);
  await expect(view.locator('li[data-repo$="/tmp"]')).toHaveCount(0);
  await expect(view.locator('li[data-project="gamma"]')).toBeVisible();
  await view.locator('li[data-repo$="/alpha"] button[name=create-project]').click();
  await expect(view.locator('li[data-project="alpha"]')).toBeVisible();
  await expect(discovered).toHaveCount(1);
  expect(cli("projects", "list")).toContain("alpha");
});
