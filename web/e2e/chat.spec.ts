// Scenario: Approval from the browser (web-dashboard, Structured chat over
// the client protocol). The mock provider answers "clean" with a terminal
// call that needs approval; the browser renders the card and answers it
// with approval.respond over /api/rpc.
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("an approval is rendered and answered from the browser", async ({ page }) => {
  const url = await signIn(page);
  const frames: string[] = [];
  page.on("websocket", (socket) => {
    if (socket.url().endsWith("/api/rpc")) socket.on("framesent", (frame) => frames.push(String(frame.payload)));
  });
  await page.goto(`${url}chat`);
  await page.getByLabel("Message").fill("clean the build");
  await page.getByRole("button", { name: "Send" }).click();
  const card = page.getByRole("article", { name: "Approval request" });
  await expect(card).toBeVisible();
  await expect(card).toContainText("rm -rf build");
  await card.getByRole("button", { name: "Deny" }).click();
  await expect(card.getByRole("status")).toHaveText("Denied");
  await expect(page.locator(".message.assistant").last()).toContainText("refused");
  expect(frames.some((f) => f.includes('"approval.respond"') && f.includes('"deny"'))).toBe(true);
  expect(frames.every((f) => !f.includes("pty") && !f.includes("shell"))).toBe(true);
});
