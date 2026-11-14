// Scenario: Invalid value (web-dashboard, Config editing is
// schema-driven). The form comes from the running build's schema; a
// refused save names the allowed values and leaves config.toml unchanged.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { env, signIn } from "./helpers";

test("an invalid value is refused and the file is unchanged", async ({ page }) => {
  const url = await signIn(page);
  const file = join(env("PMT_HOME"), "profiles/default/config.toml");
  const before = readFileSync(file);
  await page.goto(`${url}config`);
  await page.locator('[data-section="busy"] summary').click();
  const field = page.getByLabel("busy.input_mode");
  await expect(field).toHaveValue("queue");
  await field.fill("maybe");
  await page.getByRole("button", { name: "Save" }).click();
  const alert = page.getByRole("alert");
  await expect(alert).toContainText("Not saved");
  for (const allowed of ["queue", "steer", "interrupt"]) await expect(alert).toContainText(allowed);
  expect(readFileSync(file).equals(before)).toBe(true);
  await expect(page.locator(".applies", { hasText: "applies next session" }).first()).toBeAttached();
});
