// Scenario: Removing a bad lesson (web-dashboard, Experience library view).
// The domain lists its lesson with the task it came from and the adopted
// harness; Disable keeps it from new sessions (its times-shown count stays
// put when a session starts in the domain) and Delete removes it.
// crates/prometheus/tests/dashboard_e2e.rs checks a session started
// afterwards gets no lesson.
import { expect, test } from "@playwright/test";
import { cli, signIn } from "./helpers";

interface Entry {
  entry_id: string;
  times_shown: number;
  status: string;
}

const entries = (): Entry[] => JSON.parse(cli("--json", "experience", "list", "--all")) as Entry[];

test("a disabled lesson is not offered and a deleted one is gone", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}experience`);
  const domain = page.locator('section.domain[data-domain="accounting"]');
  await expect(domain).toBeVisible();
  await expect(domain.locator(".harnesses")).toContainText("Adopted harnesses: ");
  const lesson = domain.locator("li.lesson");
  await expect(lesson).toHaveCount(1);
  await expect(lesson.locator(".text")).toContainText("state the total with two decimals");
  await expect(lesson.locator(".source")).toContainText('From task "TASK-SUM the ledger"');
  const shown = entries()[0].times_shown;

  await lesson.getByRole("button", { name: "Disable" }).click();
  await expect(domain.locator('li.lesson[data-status="disabled"]')).toHaveCount(1);
  cli("-z", "hello", "--domain", "accounting");
  expect(entries()[0]).toMatchObject({ status: "disabled", times_shown: shown });

  await domain.locator("li.lesson").getByRole("button", { name: "Delete" }).click();
  await expect(page.locator("li.lesson")).toHaveCount(0);
  expect(entries()).toHaveLength(0);
});
