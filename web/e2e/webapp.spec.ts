// Scenario: Opening from the CLI; Scenario: Sign out every tab
// (web-dashboard, Desktop workspace in the browser; Webapp sign-out and
// insecure-context notice).
import { expect, test } from "@playwright/test";
import { cli } from "./helpers";

interface Webapp {
  url: string;
  login_url: string;
}

function webapp(): Webapp {
  return JSON.parse(cli("--json", "webapp", "--no-open")) as Webapp;
}

test("the CLI's one-use link opens the workspace", async ({ page }) => {
  const opened = webapp();
  await page.goto(opened.login_url);
  await expect(page).toHaveURL(/\/app$/);
  await expect(page.getByRole("heading", { name: "Workspace" })).toBeVisible();
  await expect(page.locator('[data-dropzone="staging"]')).toBeVisible();
  const again = await page.request.get(opened.login_url, { maxRedirects: 0 });
  expect(again.status()).toBe(401);
});

test("signing out from the CLI shows every tab the sign-in instructions", async ({ browser }) => {
  const first = await browser.newContext();
  const second = await browser.newContext();
  const tabs = [await first.newPage(), await first.newPage(), await second.newPage()];
  for (const tab of tabs) {
    await tab.goto(webapp().login_url);
    await expect(tab.getByRole("heading", { name: "Workspace" })).toBeVisible();
  }
  cli("webapp", "--sign-out");
  for (const tab of tabs) {
    await expect(tab.getByRole("heading", { name: "Sign in to Prometheus" })).toBeVisible();
    const call = await tab.request.get(`${new URL(tab.url()).origin}/api/profiles`);
    expect(call.status()).toBe(401);
  }
  await first.close();
  await second.close();
});
