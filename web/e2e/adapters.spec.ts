// Channel setup forms' actions (spec: web-dashboard, Channel setup forms):
// Test and Restart adapter, and WhatsApp QR pairing streamed as refreshed
// QR images until the stand-in bridge links (driven by
// crates/prometheus/tests/dashboard_browser.rs).
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("Test and Restart act on one adapter", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}adapters`);
  const webhook = page.locator('details[data-platform="webhook"]');
  await webhook.locator("summary").click();
  await webhook.getByRole("button", { name: "Test" }).click();
  await expect(webhook.locator(".adapter-actions [role=status]")).toHaveText("Test passed (inbound only)");
  await webhook.getByRole("button", { name: "Restart adapter" }).click();
  await expect(webhook.locator(".adapter-actions [role=status]")).toHaveText("Restarted: disabled");
});

test("WhatsApp pairing shows refreshed QR images until linked", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}adapters`);
  const whatsapp = page.locator('details[data-platform="whatsapp"]');
  await whatsapp.locator("summary").click();
  await whatsapp.getByRole("button", { name: "Link with QR" }).click();
  const qr = whatsapp.locator("img.pairing-qr");
  await expect(qr).toBeVisible();
  const first = await qr.getAttribute("src");
  expect(first).toMatch(/^data:image\/svg\+xml;base64,/);
  await expect.poll(async () => qr.getAttribute("src")).not.toBe(first);
  await expect(whatsapp.locator(".pairing [role=status]")).toHaveText(/^Linked as Ana/);
  await expect(whatsapp.getByRole("button", { name: "Cancel" })).toBeHidden();
});
