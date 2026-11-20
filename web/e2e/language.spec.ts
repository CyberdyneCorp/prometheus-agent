// The dashboard's language switcher (spec: i18n, Language switchers show
// endonyms), driven by crates/prometheus/tests/dashboard_browser.rs.
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("the dashboard lists languages by endonym and mirrors for Arabic", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}sessions`);
  const picker = page.locator("header label.language-picker select");
  await expect(picker.locator("option[value=ar]")).toHaveText("العربية (ar)");
  await expect(picker).toHaveValue("en");
  await picker.selectOption("ar");
  await expect(page.locator("html")).toHaveAttribute("dir", "rtl");
  // The profile's language now: a reload comes back mirrored.
  await page.reload();
  await expect(page.locator("header label.language-picker select")).toHaveValue("ar");
  await expect(page.locator("html")).toHaveAttribute("dir", "rtl");
});
