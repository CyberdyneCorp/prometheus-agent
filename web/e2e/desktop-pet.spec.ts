// The desktop pet (pets, Desktop pet and pop-out overlay; Pet slash
// command; Generating a pet; Affection reactions; Achievement unlocks
// celebrate). crates/prometheus/tests/pets_e2e.rs installs the pets and,
// for the unlock, seeds nine night sessions.
import { expect, type Page, test } from "@playwright/test";
import { nativeCalls, openDesktop, say, sent, type Sockets } from "./desktop";

const submitted = (sockets: Sockets) => sent(sockets, "prompt.submit").map((f) => f.params as { session_id: string; text: string });

/** Marks the main window hidden (the app minimised). */
async function minimise(page: Page): Promise<void> {
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

// Scenario: Prompt from the overlay.
test("the pet resizes, pops out and sends a prompt from the overlay", async ({ browser }) => {
  const context = await browser.newContext();
  const main = await context.newPage();
  const mainSockets = await openDesktop(main);
  const pet = main.locator(".floating-pet");
  const canvas = pet.locator("canvas");
  await expect(pet).toBeVisible();
  await expect(canvas).toHaveAttribute("data-scale", "0.33");
  // Alt + wheel resizes it, and the scale is the profile's pets.scale.
  await canvas.hover();
  await main.keyboard.down("Alt");
  await main.mouse.wheel(0, -100);
  await main.mouse.wheel(0, -100);
  await main.keyboard.up("Alt");
  await expect(canvas).toHaveAttribute("data-scale", "0.43");
  const session = await say(main, "hello");
  await expect.poll(() => submitted(mainSockets).length).toBe(1);

  // Shift + click: the overlay window opens and the pet leaves the window.
  await pet.click({ modifiers: ["Shift"] });
  await expect(pet).toBeHidden();
  await expect.poll(nativeCalls).toContain("pet_overlay true");
  const overlay = await context.newPage();
  const overlaySockets = await openDesktop(overlay, "pet", "#pet");
  await expect(overlay.locator(".pet-overlay canvas")).toHaveAttribute("data-scale", "0.43");

  await minimise(main);
  const before = (await nativeCalls()).length;
  await overlay.locator(".pet-body").click();
  await overlay.fill("input[name=pet-prompt]", "check the build");
  await overlay.press("input[name=pet-prompt]", "Enter");
  await expect.poll(() => submitted(mainSockets).map((p) => p.text)).toContain("check the build");
  const prompt = submitted(mainSockets).find((p) => p.text === "check the build");
  expect(prompt?.session_id).toBe(session);
  // The overlay never connected; the app was not brought back.
  expect(overlaySockets.length).toBe(0);
  const since = (await nativeCalls()).slice(before);
  expect(since.filter((c) => c.startsWith("main_window") || c.startsWith("window "))).toEqual([]);
  expect(since).toContain("emit main pet-message");
  // The turn finished while the app was hidden: the mail badge.
  await expect(overlay.locator(".pet-mail")).toBeVisible();
  // A double-click shows the main window again.
  await overlay.locator(".pet-body").dblclick();
  await expect.poll(nativeCalls).toContain("main_window toggle");
  await expect(overlay.locator(".pet-mail")).toBeHidden();
  // Back into the window.
  await overlay.locator("button[name=pet-back]").click();
  await expect.poll(nativeCalls).toContain("pet_overlay false");
  await expect(pet).toBeVisible();
});
