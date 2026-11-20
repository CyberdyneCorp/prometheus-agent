// The desktop status bar on a managed local model (spec: local-models,
// Local resource telemetry), driven by crates/prometheus/tests/
// desktop_local.rs against the real daemon on the `pmt-llama-stub` engine
// with a 24 GiB GPU hardware fixture.
import { expect, test } from "@playwright/test";
import { openDesktop, say } from "./desktop";

test("the status bar shows the local runtime's GPU and RAM use", async ({ page }) => {
  await openDesktop(page);
  await say(page, "hello");
  await expect(page.locator(".transcript .message.assistant")).toHaveCount(1, { timeout: 60_000 });
  await expect(page.locator("footer.status-bar [name=local-gpu]")).toHaveText("GPU 37% 2.0/24.0 GiB");
  await expect(page.locator("footer.status-bar [name=local-ram]")).toHaveText("RAM 16.0/32.0 GiB");
  // A hidden item stays hidden whatever the session reports.
  await page.locator("footer.status-bar").click({ button: "right" });
  await page.locator(".status-menu input[name=item-resources]").uncheck();
  await expect(page.locator("footer.status-bar [name=local-gpu]")).toBeHidden();
});
