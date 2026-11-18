// The desktop's language switcher (spec: i18n, Language switchers show
// endonyms; Scenario: RTL layout), driven by crates/prometheus/tests/
// desktop_e2e.rs against the real daemon.
import { expect, test } from "@playwright/test";
import { openDesktop, sent } from "./desktop";

test("choosing Arabic mirrors the window without a restart", async ({ page }) => {
  const sockets = await openDesktop(page);
  const picker = page.locator("label.language-picker select");
  await expect(picker.locator("option[value=ar]")).toHaveText("العربية (ar)");
  await expect(picker.locator("option[value=de]")).toHaveText("Deutsch (de)");
  await expect(picker).toHaveValue("en");
  await expect(page.locator("html")).toHaveAttribute("dir", "ltr");
  const before = await page.locator("nav.side").boundingBox();
  await picker.selectOption("ar");
  await expect(page.locator("html")).toHaveAttribute("dir", "rtl");
  await expect(page.locator("html")).toHaveAttribute("lang", "ar");
  const patches = sent(sockets, "config.patch");
  expect(patches.at(-1)?.params).toEqual({ profile: "default", changes: [{ key: "display.language", value: "ar" }] });
  // The same window, mirrored: the sessions column moved to the other side.
  const after = await page.locator("nav.side").boundingBox();
  expect(after?.x ?? 0).toBeGreaterThan(before?.x ?? 0);
  // And back.
  await picker.selectOption("en");
  await expect(page.locator("html")).toHaveAttribute("dir", "ltr");
});
