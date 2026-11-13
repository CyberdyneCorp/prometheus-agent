// Scenario: Reading a key (web-dashboard, Secrets are write-only).
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("a secret set in the page is listed by name only", async ({ page }) => {
  const url = await signIn(page);
  const value = "sk-written-from-the-browser-9876";
  await page.goto(`${url}secrets`);
  await page.getByLabel("Secret name").fill("BROWSER_TOKEN");
  await page.getByLabel("Secret value").fill(value);
  await page.getByRole("button", { name: "Set" }).click();
  await expect(page.locator('[data-secret="BROWSER_TOKEN"]')).toBeVisible();
  await expect(page.getByLabel("Secret value")).toHaveValue("");
  await page.reload();
  await expect(page.locator('[data-secret="BROWSER_TOKEN"]')).toBeVisible();
  expect(await page.content()).not.toContain(value);
  const listed = await page.request.get(`${url}api/profiles/default/secrets`);
  expect(await listed.text()).not.toContain(value);
});
