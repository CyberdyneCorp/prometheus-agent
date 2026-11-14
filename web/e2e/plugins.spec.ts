// Dashboard plugin contributions in the browser (spec: web-dashboard,
// Dashboard plugin contributions): Scenario: Override conflict; Scenario:
// Plugin frame reads the session cookie; plugin pages, slots and backend
// calls through the plugin UI SDK; a plugin that never registers does not
// hold up the page it overrides. The home's plugins are placed by
// crates/prometheus/tests/common/dashboard_plugins.rs.
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

test("the first override in load order renders and the other is reported", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}sessions`);
  const frame = page.locator('iframe[data-plugin="a"][data-slot="page"]');
  await expect(frame).toHaveAttribute("data-state", "registered");
  await expect(page.frameLocator('iframe[data-plugin="a"]').locator("h1")).toHaveText("Plugin A page");
  await expect(page.frameLocator('iframe[data-plugin="a"]').locator("#context")).toHaveText("route /sessions slot page");
  await expect(page.locator('iframe[data-plugin="b"]')).toHaveCount(0);
  await page.goto(`${url}plugins`);
  await expect(page.locator('[data-ignored="b"]')).toContainText("override of /sessions was ignored (a renders it");
});

test("a plugin frame never sees the session cookie", async ({ page }) => {
  const url = await signIn(page);
  const cookies = await page.context().cookies();
  expect(cookies.some((c) => c.name === "prometheus_session")).toBe(true);
  await page.goto(`${url}sessions`);
  const shown = page.frameLocator('iframe[data-plugin="a"]').locator("#cookie");
  await expect(shown).toHaveText(/^cookie: /);
  expect(await shown.textContent()).not.toContain("prometheus_session");
  const sandbox = (await page.locator('iframe[data-plugin="a"]').getAttribute("sandbox")) ?? "";
  expect(sandbox.split(/\s+/)).toEqual(["allow-scripts"]);
});

test("plugin pages, slots and backend calls work through the SDK", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}status`);
  const links = page.locator("nav a");
  await expect(links.filter({ hasText: "Weather" })).toHaveCount(1);
  const labels = await links.allTextContents();
  expect(labels.indexOf("Weather")).toBe(labels.indexOf("Sessions") + 1);
  expect(labels).not.toContain("Plugin A");
  await expect(page.locator('[data-shell-slot="header-right"] iframe[data-plugin="weather"]')).toHaveCount(1);
  await links.filter({ hasText: "Weather" }).click();
  const weather = page.frameLocator('main iframe[data-plugin="weather"]');
  await expect(weather.locator("#forecast")).toHaveText("forecast sunny");
});

test("a plugin that never registers does not hold up the page it overrides", async ({ page }) => {
  const url = await signIn(page);
  const started = Date.now();
  await page.goto(`${url}logs`);
  await expect(page.getByText("Mute did not respond; showing the built-in page.")).toBeVisible({ timeout: 5000 });
  expect(Date.now() - started).toBeLessThan(5000);
  await expect(page.getByRole("heading", { name: "Logs" })).toBeVisible();
  await page.locator("nav a", { hasText: "Plugins" }).click();
  await expect(page.getByRole("row", { name: /^mute .*did not register within 2 s$/ })).toHaveCount(1);
});

test("a plugin frame cannot reach other dashboard routes through dot segments", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}weather`);
  const weather = page.frameLocator('main iframe[data-plugin="weather"]');
  await expect(weather.locator("#forecast")).toHaveText("forecast sunny");
  const frame = page.frames().find((f) => f.url().includes("/api/plugin-frames/weather"));
  expect(frame).toBeDefined();
  const sent: string[] = [];
  page.on("request", (r) => sent.push(new URL(r.url()).pathname));
  const escapes = [
    "/%2e%2e/%2e%2e/profiles/default/secrets/STOLEN",
    "/.%2e/.%2E/profiles/default/secrets/STOLEN",
    "/%2E%2e/rescan",
    "/..\\..\\profiles/default/config",
    "/./../../profiles/default/config",
  ];
  for (const path of escapes) {
    const answer = await frame!.evaluate(
      (p) =>
        (window as unknown as { prometheus: { api: (m: string, p: string, b?: unknown) => Promise<unknown> } }).prometheus
          .api("PUT", p, { value: "x" })
          .then(
            () => "sent",
            (e: Error) => e.message,
          ),
      path,
    );
    expect(answer, path).toBe("bad backend request");
  }
  const forecast = await frame!.evaluate(() =>
    (window as unknown as { prometheus: { api: (m: string, p: string) => Promise<{ forecast: string }> } }).prometheus
      .api("GET", "/forecast")
      .then((x) => x.forecast),
  );
  expect(forecast).toBe("sunny");
  // Routine page traffic may run meanwhile; none of the targets was hit.
  const reached = sent.filter((p) => /\/(secrets|config)(\/|$)|\/rescan$/.test(p));
  expect(reached).toEqual([]);
});
