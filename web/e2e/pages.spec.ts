// Built-in pages (web-dashboard): every built-in route renders its page,
// and an unknown route a not-found page linking to /sessions.
import { expect, test } from "@playwright/test";
import { signIn } from "./helpers";

const ROUTES = [
  "/chat", "/sessions", "/projects", "/usage", "/models", "/skills", "/plugins", "/mcp", "/cron", "/adapters",
  "/pairing", "/webhooks", "/memory", "/logs", "/status", "/profiles", "/config", "/secrets",
  "/kanban", "/evolution", "/experience", "/files", "/docs", "/profile-builder", "/themes", "/achievements",
];

test("every built-in route renders and an unknown one is not found", async ({ page }) => {
  const url = await signIn(page);
  for (const route of ROUTES) {
    await page.goto(`${url}${route.slice(1)}`);
    await expect(page.locator("section.page h1"), route).toBeVisible();
    await expect(page.locator("section.page h1"), route).not.toHaveText("Page not found");
  }
  await page.goto(`${url}no-such-page`);
  await expect(page.locator("section.page h1")).toHaveText("Page not found");
  await page.getByRole("link", { name: "Go to sessions" }).click();
  await expect(page).toHaveURL(/\/sessions$/);
  await expect(page.locator("section.page h1")).toHaveText("Sessions");
});
