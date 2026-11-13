// Scenario: Secret shown once (web-dashboard, Webhooks page); Scenario:
// Offline docs (web-dashboard, Embedded documentation).
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("a new webhook's secret is shown once and gone after a reload", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}webhooks`);
  await expect(page.locator('[data-adapter="off"]')).toContainText("disabled");
  await page.getByLabel("Name").fill("deploys");
  await page.getByLabel("Events (comma-separated)").fill("push");
  await page.getByRole("button", { name: "Create" }).click();
  const secret = page.locator("[data-secret]");
  await expect(secret).toHaveText(/^[0-9a-f]{32,}$/);
  const value = await secret.textContent();
  await expect(page.getByRole("cell", { name: "deploys", exact: true })).toBeVisible();
  await page.reload();
  await expect(page.getByRole("cell", { name: "deploys", exact: true })).toBeVisible();
  await expect(page.locator("[data-secret]")).toHaveCount(0);
  expect(await page.content()).not.toContain(value ?? "never");
});

test("the docs answer a config key without leaving the machine", async ({ page }) => {
  const url = await signIn(page);
  const origin = new URL(url).origin;
  const outside: string[] = [];
  await page.route("**/*", (route) => {
    if (!route.request().url().startsWith(origin)) {
      outside.push(route.request().url());
      return route.abort();
    }
    return route.continue();
  });
  await page.goto(`${url}docs`);
  await page.getByLabel("Search the documentation").fill("busy.input_mode");
  await page.getByRole("button", { name: "Search" }).click();
  const entry = page.locator('[data-entry="busy.input_mode"]');
  await expect(entry.getByRole("heading", { name: "busy.input_mode" })).toBeVisible();
  await expect(entry).toContainText("queue");
  expect(outside).toEqual([]);
});
