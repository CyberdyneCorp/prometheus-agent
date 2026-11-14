// Gateway switching and scoping (spec: desktop-app): Approval from a
// background connection, Remote connection keeps local preferences
// (Machine-wide UI preferences) and Failed switch is not remembered. The
// harness registers `Laptop` (the local daemon), `Homelab` (another
// daemon's dashboard listener, paired) and `VPS` (offline).
import { expect, type Page, test } from "@playwright/test";
import { openDesktop, say } from "./desktop";

const status = (page: Page) => page.locator(".connection");

async function show(page: Page, connection: string, expected: RegExp): Promise<void> {
  await page.getByLabel("Connection", { exact: true }).selectOption({ label: connection });
  await expect(status(page)).toHaveText(expected, { timeout: 30_000 });
}

test("an approval of a session on another connection is answered on that connection", async ({ page }) => {
  const sockets: { url: string; frames: string[] }[] = [];
  page.on("websocket", (ws) => {
    const entry = { url: ws.url(), frames: [] as string[] };
    sockets.push(entry);
    ws.on("framesent", (frame) => entry.frames.push(String(frame.payload)));
  });
  await openDesktop(page);
  await expect(status(page)).toHaveText("Connected · default");
  await show(page, "Homelab", /Connected · default on Homelab/);
  const session = await say(page, "later clean the build");
  // The window goes back to Laptop while Homelab's turn still runs.
  await show(page, "Laptop", /^Connected · default$/);
  const other = page.getByRole("region", { name: "Other connections" });
  const card = other.getByRole("article", { name: "Approval request" });
  await expect(card).toBeVisible({ timeout: 30_000 });
  await expect(other.locator(".origin")).toHaveText("Homelab · default");
  await card.getByRole("button", { name: "Approve once" }).click();
  await expect(card.getByRole("status")).toHaveText("Approved (once)");
  const answered = sockets.filter((s) => s.frames.some((f) => f.includes('"approval.respond"')));
  expect(answered).toHaveLength(1);
  // It went on the connection that follows the Homelab session, not on
  // the window's Laptop connection: the window's first socket is the
  // local daemon's, Homelab's go through the shell's forwarder.
  const local = sockets[0]?.url;
  const homelab = answered[0];
  expect(homelab?.url).not.toBe(local);
  expect(homelab?.frames.some((f) => f.includes('"session.attach"') && f.includes(session))).toBe(true);
  const laptop = sockets.filter((s) => s.url === local);
  expect(laptop.some((s) => s.frames.some((f) => f.includes('"approval.respond"')))).toBe(false);
});

test("an accent chosen while a remote connection shows is stored on this machine", async ({ page }) => {
  await openDesktop(page);
  await show(page, "Homelab", /Connected · default on Homelab/);
  await page.getByLabel("Accent colour").fill("#123456");
  await expect(page.locator("html")).toHaveAttribute("style", /--accent: #123456/);
});

test("a failed switch is not remembered for the next launch", async ({ page }) => {
  await openDesktop(page);
  await page.getByLabel("Open on launch").selectOption("last_used");
  await show(page, "Homelab", /Connected · default on Homelab/);
  await page.getByLabel("Connection", { exact: true }).selectOption({ label: "VPS" });
  await expect(page.locator(".notices")).toContainText("Could not connect to VPS", { timeout: 30_000 });
  await expect(status(page)).not.toHaveText(/Connected/);
  // The app restarts: it opens on the last connection that succeeded.
  await page.reload();
  await expect(status(page)).toHaveText(/Connected · default on Homelab/, { timeout: 60_000 });
  await expect(page.getByLabel("Connection", { exact: true })).toHaveValue("homelab");
});

test("connection groups are renamed and reordered on this device only", async ({ page }) => {
  await openDesktop(page);
  const groups = page.getByRole("navigation", { name: "Connections and profiles" });
  await groups.getByRole("button", { name: "Rename Homelab" }).click();
  const name = groups.getByLabel("New name for Homelab");
  await name.fill("Lab rack");
  await name.press("Enter");
  await groups.getByRole("button", { name: "Move up Homelab" }).click();
  const picker = page.getByLabel("Connection", { exact: true });
  await expect(picker.locator("option").first()).toHaveText("Lab rack");
  // The registry keeps its own name: the group label is display state.
  await expect(page.locator('.connection-list li[data-connection="homelab"] .name')).toHaveText("Homelab");
  await page.reload();
  await expect(page.getByLabel("Connection", { exact: true }).locator("option").first()).toHaveText("Lab rack");
});
