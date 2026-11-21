// Scenario: Unlock during a session (pets, Achievement unlocks celebrate):
// the desktop's toast and the pet's jump. crates/prometheus/tests/
// pets_e2e.rs seeds nine night sessions, so this chat is the tenth.
import { expect, test } from "@playwright/test";
import { openDesktop, say } from "./desktop";

test("an unlock shows a toast and the pet jumps", async ({ page }) => {
  await openDesktop(page);
  await say(page, "hello");
  const toast = page.locator('.achievement-toast[data-id="night-owl"]');
  await expect(toast).toContainText("Night Owl · silver", { timeout: 30_000 });
  await expect(page.locator(".floating-pet")).toHaveAttribute("data-state", "jump");
  await toast.locator("button[name=dismiss]").click();
  await expect(toast).toBeHidden();
});
