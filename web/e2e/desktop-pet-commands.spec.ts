// /pet, /hatch and the heart on the desktop (pets, Pet slash command;
// Generating a pet; Affection reactions): client-side, never a turn.
// crates/prometheus/tests/pets_e2e.rs installs the pets.
import { expect, test } from "@playwright/test";
import { openDesktop, sent } from "./desktop";

test("/pet and /hatch run in the window and never start a turn", async ({ page }) => {
  const sockets = await openDesktop(page);
  const pet = page.locator(".floating-pet");
  await expect(pet).toBeVisible();
  const command = async (text: string) => {
    await page.fill("textarea[name=prompt]", text);
    await page.click("form.composer button[type=submit]");
  };
  await command("/pet off");
  await expect(pet).toBeHidden();
  await command("/pet mochi");
  await expect(pet).toBeVisible();
  await command("/pet list");
  const list = page.locator(".pets-dialog");
  await expect(list.locator('li[data-slug="boba"] button')).toHaveText("Adopt");
  await expect(list.locator('li[data-slug="taro"] button')).toHaveText("Install");
  await list.locator('li[data-slug="taro"] button').click();
  await expect(list).toBeHidden();
  await expect.poll(async () => (await page.evaluate(() => document.querySelector(".floating-pet canvas")?.getAttribute("data-state"))) !== null).toBe(true);
  await command("/hatch a sleepy fox");
  const hatch = page.locator(".hatch-dialog");
  await expect(hatch).toHaveAttribute("data-state", "failed");
  await expect(hatch.locator(".hatch-text")).toContainText("pet generation needs an image backend with reference-image support");
  expect(sent(sockets, "prompt.submit")).toEqual([]);
  // A thank-you plays the heart and goes as typed.
  await command("thanks, good bot");
  await expect(pet.locator(".pet-heart")).toBeVisible();
  await expect.poll(() => sent(sockets, "prompt.submit").map((f) => (f.params as { text: string }).text)).toEqual(["thanks, good bot"]);
});
