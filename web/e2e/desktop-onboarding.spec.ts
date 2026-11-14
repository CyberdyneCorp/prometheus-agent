// The first-run onboarding chat (spec: desktop-app, First-run onboarding
// chat), driven by crates/prometheus/tests/desktop_ui.rs on a machine that
// was never set up.
import { readFileSync } from "node:fs";
import { expect, type Page, test } from "@playwright/test";
import { openDesktop, say } from "./desktop";
import { env } from "./helpers";

const answers = (page: Page) => page.locator(".transcript .message.assistant .text");

test("the first launch opens onboarding and only its chat shows setup cards", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  const name = page.locator('.setup-card[data-beat="name"]');
  await expect(name).toBeVisible({ timeout: 60_000 });
  await expect(page.locator("button[name=skip-setup]")).toBeVisible();
  await name.locator("input[name=setup-answer]").fill("Leo");
  await name.locator("button[name=setup-send]").click();
  const theme = page.locator('.setup-card[data-beat="theme"]');
  await expect(theme).toBeVisible({ timeout: 60_000 });
  await theme.locator('button[data-option="dark"]').click();
  await expect(answers(page).last()).toHaveText("Welcome, Leo", { timeout: 60_000 });

  // A regular chat's forged setup call is refused: no card shows.
  await page.click("button[name=new-chat]");
  await say(page, "forge a setup card");
  await expect(answers(page).last()).toHaveText("ok", { timeout: 60_000 });
  await expect(page.locator('.setup-card[data-beat="layout"]')).toHaveCount(0);

  await page.click("button[name=skip-setup]");
  await expect(page.locator("button[name=skip-setup]")).toBeHidden();
  await expect.poll(() => readFileSync(`${env("PMT_HOME")}/ui-prefs.json`, "utf8")).toMatch(/"onboarding":\s*"skipped"/);
});
