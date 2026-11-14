// Built-in pages (web-dashboard): /plugins, /mcp, /cron and /memory show
// the addressed profile's plugins, MCP servers, cron jobs and memory
// entries as the daemon has them.
import { expect, test } from "@playwright/test";
import { env, signIn } from "./helpers";

const PAGES: [string, string, string][] = [
  ["plugins", "Plugins", "weather"],
  ["mcp", "MCP", "docs"],
  ["cron", "Cron", env("PMT_CRON_JOB")],
  ["memory", "Memory", "The deploy window is Tuesday."],
];

test("the plugin, MCP, cron and memory pages list the profile's data", async ({ page }) => {
  const url = await signIn(page);
  for (const [route, title, row] of PAGES) {
    await page.goto(`${url}${route}`);
    await expect(page.locator("section.page h1"), route).toHaveText(title);
    await expect(page.locator("section.page table"), route).toContainText(row);
  }
});
