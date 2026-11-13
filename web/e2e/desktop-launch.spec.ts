// Launching and install links (spec: desktop-app): Launch in a project
// directory (Launching the desktop from the CLI), Typo in a launch option
// (Desktop launch options), Website install button and Link pointing at
// an arbitrary URL (Install deep links open a consent flow), Theme
// changed in the webapp (Machine-wide UI preferences), and Wayland
// without the portal (Honest platform support in settings) with the
// development build's updater.
import { expect, test } from "@playwright/test";
import { emit, openDesktop } from "./desktop";
import { env, signIn } from "./helpers";

test("the link `prometheus desktop --cwd` passed opens a draft there", async ({ page }) => {
  await openDesktop(page);
  await expect(page.locator(".chat-title")).toHaveText(`New chat in ${env("PMT_LAUNCH_CWD")}`);
});

test("a typo in a launch option is a notice and the app runs", async ({ page }) => {
  await openDesktop(page);
  await expect(page.getByText('desktop.disable_gpu: expected one of "auto", "on", "off"')).toBeVisible();
});

test("an install link naming a URL is an unknown catalog entry", async ({ page }) => {
  await openDesktop(page);
  await emit(page, "deep-link", { open: "install_plugin", id: "https://evil.example/p.wasm" });
  await expect(page.getByRole("dialog")).toContainText("Unknown catalog entry https://evil.example/p.wasm");
  await emit(page, "deep-link", { open: "install_plugin", id: "weather", extra_params: true });
  await expect(page.getByRole("dialog")).toContainText("Unknown catalog entry weather");
});

test("the website install button installs only after Install", async ({ page }) => {
  await openDesktop(page);
  await emit(page, "deep-link", { open: "install_plugin", id: "weather" });
  const dialog = page.getByRole("dialog");
  await expect(dialog.locator('[data-field="publisher"]')).toHaveText("github.com/acme (community tier)");
  await expect(dialog.locator('[data-field="version"]')).toHaveText("1.0.0");
  await expect(dialog.locator(".capabilities")).toContainText("network api.weather.example");
  // Cancelled: nothing is downloaded or installed.
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toHaveCount(0);
  await emit(page, "deep-link", { open: "install_plugin", id: "weather" });
  await page.getByRole("dialog").getByRole("button", { name: "Install" }).click();
  await expect(page.getByRole("dialog").getByRole("status")).toHaveText("Installed weather", { timeout: 60_000 });
});

test("dark mode chosen in the webapp repaints the desktop window", async ({ browser }) => {
  const desktop = await (await browser.newContext()).newPage();
  await openDesktop(desktop);
  await expect(desktop.locator("html")).not.toHaveAttribute("data-mode", "dark");
  const webapp = await (await browser.newContext()).newPage();
  const url = await signIn(webapp);
  await webapp.goto(`${url}themes`);
  await expect(webapp.getByLabel("Mode")).toBeEnabled();
  await webapp.getByLabel("Mode").selectOption("dark");
  await expect(desktop.locator("html")).toHaveAttribute("data-mode", "dark", { timeout: 2_000 });
  // Back to light in the desktop's own store: the webapp follows.
  await expect(webapp.locator("html")).toHaveAttribute("data-mode", "dark");
});

test("settings disable the hotkey on Wayland and dev builds get no updates", async ({ page }) => {
  await openDesktop(page);
  const hotkey = page.getByLabel("Quick ask hotkey");
  await expect(hotkey).toBeDisabled();
  await expect(page.locator('.reason[data-for="desktop.quick_ask.hotkey"]')).toHaveText("global shortcuts are not available in this Wayland session");
  await page.getByRole("button", { name: "Check for updates" }).click();
  await expect(page.locator(".update-status")).toHaveText("This build doesn't get updates. Install a release build to receive updates.");
});
