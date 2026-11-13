// Scenario: Opening from the CLI; Scenario: Sign out every tab
// (web-dashboard, Desktop workspace in the browser; Webapp sign-out and
// insecure-context notice); URL previews load in a sandboxed frame that
// cannot open popups (Webapp file staging and terminals).
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { expect, test } from "@playwright/test";
import { cli } from "./helpers";

interface Webapp {
  url: string;
  login_url: string;
}

function webapp(): Webapp {
  return JSON.parse(cli("--json", "webapp", "--no-open")) as Webapp;
}

test("the CLI's one-use link opens the workspace", async ({ page }) => {
  const opened = webapp();
  await page.goto(opened.login_url);
  await expect(page).toHaveURL(/\/app$/);
  await expect(page.getByRole("heading", { name: "Workspace" })).toBeVisible();
  await expect(page.locator('[data-dropzone="staging"]')).toBeVisible();
  const again = await page.request.get(opened.login_url, { maxRedirects: 0 });
  expect(again.status()).toBe(401);
});

test("signing out from the CLI shows every tab the sign-in instructions", async ({ browser }) => {
  const first = await browser.newContext();
  const second = await browser.newContext();
  const tabs = [await first.newPage(), await first.newPage(), await second.newPage()];
  for (const tab of tabs) {
    await tab.goto(webapp().login_url);
    await expect(tab.getByRole("heading", { name: "Workspace" })).toBeVisible();
  }
  cli("webapp", "--sign-out");
  for (const tab of tabs) {
    await expect(tab.getByRole("heading", { name: "Sign in to Prometheus" })).toBeVisible();
    const call = await tab.request.get(`${new URL(tab.url()).origin}/api/profiles`);
    expect(call.status()).toBe(401);
  }
  await first.close();
  await second.close();
});

test("a URL preview loads in a sandboxed frame that cannot open popups", async ({ page }) => {
  const site = createServer((_, res) => {
    res.writeHead(200, { "content-type": "text/html" });
    res.end(
      "<p id=result>pending</p><script>let w=null;try{w=window.open('about:blank','_blank')}catch(e){}" +
        "document.getElementById('result').textContent=w?'popup opened':'popup blocked';</script>",
    );
  });
  await new Promise<void>((resolve) => site.listen(0, "127.0.0.1", resolve));
  const { port } = site.address() as AddressInfo;
  try {
    await page.goto(webapp().login_url);
    await expect(page.getByRole("heading", { name: "Workspace" })).toBeVisible();
    let popups = 0;
    page.context().on("page", () => popups++);
    await page.getByLabel("Preview URL").fill(`http://127.0.0.1:${port}/`);
    await page.getByRole("button", { name: "Preview" }).click();
    const frame = page.locator("iframe.preview");
    await expect(frame).toBeVisible();
    const sandbox = (await frame.getAttribute("sandbox")) ?? "";
    expect(sandbox).toContain("allow-scripts");
    expect(sandbox).not.toContain("allow-popups");
    expect(sandbox).not.toContain("allow-same-origin");
    await expect(page.frameLocator("iframe.preview").locator("#result")).toHaveText("popup blocked");
    expect(popups).toBe(0);
  } finally {
    site.close();
  }
});
