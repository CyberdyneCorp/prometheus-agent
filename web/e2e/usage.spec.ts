// Scenario: Delegation cost rolls up. Scenario: Budget warning
// (web-dashboard, Cost and usage view). The harness seeds the ledger: a
// session spent 0.10 itself and 0.40 through its subagents, another 18.00,
// against a monthly budget of 20.00.
import { expect, test } from "@playwright/test";
import { env, signIn } from "./helpers";

test("delegation cost rolls up and the budget bar shows the spend", async ({ page }) => {
  const url = await signIn(page);
  const root = env("PMT_ROOT_SESSION");
  await page.goto(`${url}usage`);
  await expect(page.locator(`[data-total="${root}"]`)).toHaveText("$0.50");
  await expect(page.locator(`[data-child-of="${root}"]`)).toHaveCount(2);
  await expect(page.locator('[data-budget="monthly_usd"] .percent')).toHaveText("92.5% used");
  const csv = await page.request.get(`${url}api/profiles/default/usage.csv`);
  expect(csv.status()).toBe(200);
  expect((await csv.text()).split("\n")[0]).toContain("cost_usd");
});
