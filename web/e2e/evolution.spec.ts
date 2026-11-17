// Scenario: Adopting a harness (web-dashboard, Harness-evolution view). A
// finished run is listed with its mode, budget, rollouts, improver sessions,
// cost and status; its detail shows the stages, the candidate diffs, the
// selector's pick and the proposal; Adopt applies it from the next session
// while a live chat session keeps going on its cached prompt. The live
// session's id goes to PMT_LIVE_SESSION_FILE, whose prompt
// crates/prometheus/tests/dashboard_e2e.rs then checks.
import { writeFileSync } from "node:fs";
import { expect, test } from "@playwright/test";
import { env, signIn } from "./helpers";

test("a finished run's proposal is adopted for new sessions only", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}chat`);
  await page.getByLabel("Message").fill("hello");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.locator(".message.assistant").last()).toContainText("hi there");
  const live = new URL(page.url()).searchParams.get("session") ?? "";
  expect(live).not.toBe("");
  writeFileSync(env("PMT_LIVE_SESSION_FILE"), live);
  const chat = page.url();

  await page.goto(`${url}evolution`);
  const view = page.locator(".evolution-page");
  const row = view.locator("tbody tr").first();
  await expect(row).toContainText("parallel");
  await expect(row).toContainText("k=1");
  await expect(row.locator("td").nth(6)).toHaveText("ok");
  await view.locator("a[data-job]").first().click();
  await expect(view.locator("p.selected")).toContainText("by single");
  await expect(view.locator("details.diff").first()).toBeAttached();
  const pending = view.locator('div.proposal[data-status="pending"]').first();
  const id = (await pending.getAttribute("data-proposal")) ?? "";
  const proposal = view.locator(`div.proposal[data-proposal="${id}"]`);
  await expect(proposal.locator("pre.proposal-diff")).toContainText("+Always state the total with two decimals.");
  await proposal.getByRole("button", { name: "Adopt" }).click();
  await expect(proposal.locator(".result")).toContainText("applies from the next session");
  await expect(proposal).toHaveAttribute("data-status", "adopted");

  await page.goto(chat);
  await page.getByLabel("Message").fill("hello again");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.locator(".message.assistant")).toHaveCount(2);
});
