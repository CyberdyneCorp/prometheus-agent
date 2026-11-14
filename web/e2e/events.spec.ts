// Scenario: Approval badge (web-dashboard, Live event stream). The
// sessions page follows /api/events; when a session of profile `work`
// starts waiting for approval, its row shows a pending badge.
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("a session waiting for approval shows a pending badge", async ({ browser }) => {
  const chat = await browser.newPage();
  const url = await signIn(chat);
  await chat.goto(`${url}chat?profile=work`);
  await chat.getByLabel("Message").fill("hello");
  await chat.getByRole("button", { name: "Send" }).click();
  await expect(chat.locator(".message.assistant")).toHaveCount(1);
  const session = new URL(chat.url()).searchParams.get("session");
  expect(session).toMatch(/^ses_/);

  const sessions = await browser.newPage();
  await signIn(sessions);
  await sessions.goto(`${url}sessions?profile=work`);
  const row = sessions.locator(`[data-badges="${session}"]`);
  await expect(row).toBeEmpty();

  await chat.getByLabel("Message").fill("clean the build");
  await chat.getByRole("button", { name: "Send" }).click();
  await expect(chat.getByRole("article", { name: "Approval request" })).toBeVisible();
  await expect(row.locator('[data-badge="approval"]')).toHaveText("approval pending");

  await chat.getByRole("button", { name: "Deny" }).click();
  await expect(row.locator('[data-badge="approval"]')).toHaveCount(0);
});
