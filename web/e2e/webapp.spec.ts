// Scenario: Opening from the CLI; Scenario: Sign out every tab;
// Scenario: Oversized drop (web-dashboard, Desktop workspace in the
// browser; Webapp sign-out and insecure-context notice; Webapp file
// staging and terminals): `/app` is the desktop app's workspace rendered
// by the desktop's own components over the dashboard listener; native-only
// features report that they need the desktop app; files are staged under
// the profile (16 MiB each); a terminal is the daemon's PTY and a second
// tab attaching takes it over; URL previews cannot open popups.
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import { expect, type Page, test } from "@playwright/test";
import { cli } from "./helpers";

interface Webapp {
  url: string;
  login_url: string;
}

function webapp(): Webapp {
  return JSON.parse(cli("--json", "webapp", "--no-open")) as Webapp;
}

/** Signs `page` in through a fresh one-use link; resolves once the
 *  desktop workspace is connected. */
async function open(page: Page): Promise<void> {
  await page.goto(webapp().login_url);
  await expect(page).toHaveURL(/\/app$/);
  await expect(page.locator("header.top .connection")).toHaveAttribute("data-state", "connected");
}

/** Starts a chat in the workspace and returns its session id. */
async function chat(page: Page, text: string): Promise<string> {
  await page.fill("textarea[name=prompt]", text);
  await page.click("form.composer button[type=submit]");
  const view = page.locator("section.chat");
  await expect(view).toHaveAttribute("data-session", /^ses_/);
  return (await view.getAttribute("data-session")) ?? "";
}

test("the CLI's one-use link opens the desktop workspace", async ({ page }) => {
  const opened = webapp();
  await page.goto(opened.login_url);
  await expect(page).toHaveURL(/\/app$/);
  await expect(page.locator("header.top .brand")).toHaveText("Prometheus");
  await expect(page.locator("header.top .connection")).toHaveAttribute("data-state", "connected");
  await expect(page.locator("nav.side")).toBeVisible();
  const again = await page.request.get(opened.login_url, { maxRedirects: 0 });
  expect(again.status()).toBe(401);
  // A chat runs on the daemon through the dashboard listener.
  await chat(page, "hello from the browser");
  await expect(page.locator(".transcript .message.assistant")).toHaveCount(1);
});

test("signing out from the CLI shows every tab the sign-in instructions", async ({ browser }) => {
  const first = await browser.newContext();
  const second = await browser.newContext();
  const tabs = [await first.newPage(), await first.newPage(), await second.newPage()];
  for (const tab of tabs) await open(tab);
  cli("webapp", "--sign-out");
  for (const tab of tabs) {
    await expect(tab.getByRole("heading", { name: "Sign in to Prometheus" })).toBeVisible();
    const call = await tab.request.get(`${new URL(tab.url()).origin}/api/profiles`);
    expect(call.status()).toBe(401);
  }
  await first.close();
  await second.close();
});

test("a terminal is the daemon's shell and a second tab takes it over", async ({ browser }) => {
  const context = await browser.newContext();
  const one = await context.newPage();
  await open(one);
  const session = await chat(one, "terminal please");
  await one.selectOption("select[name=layout]", "quad");
  await one.click("button[name=new-terminal]");
  const screen = one.locator("pre.terminal-screen");
  await expect(screen).toBeFocused();
  await one.keyboard.type("echo web-$((40+2))");
  await one.keyboard.press("Enter");
  await expect(screen).toContainText("web-42");

  const two = await context.newPage();
  await open(two);
  await two.locator(`nav.side button[data-session="${session}"]`).click();
  await two.selectOption("select[name=layout]", "quad");
  const taken = two.locator("pre.terminal-screen");
  await expect(taken).toContainText("web-42");
  await expect(one.locator("pre.terminal-screen")).toHaveAttribute("data-detached", "true");
  await expect(one.locator(".terminal-status")).toContainText("another window");
  await taken.click();
  await two.keyboard.type("echo two-$((1+1))");
  await two.keyboard.press("Enter");
  await expect(taken).toContainText("two-2");
  // The first tab's typing is refused until it takes the terminal back.
  await one.locator("pre.terminal-screen").click();
  await one.keyboard.type("x");
  await expect(one.locator(".terminal-status")).toContainText("another window");
  await one.click("button[name=take-back]");
  await expect(one.locator("pre.terminal-screen")).not.toHaveAttribute("data-detached", "true");
  await expect(two.locator("pre.terminal-screen")).toHaveAttribute("data-detached", "true");
  await context.close();
});

test("a file dropped on the terminal is staged, and a 20 MiB one is refused with 413", async ({ page }) => {
  await open(page);
  await chat(page, "files please");
  await page.selectOption("select[name=layout]", "quad");
  await page.click("button[name=new-terminal]");
  const screen = page.locator("pre.terminal-screen");
  const drop = async (name: string, size: number) => {
    const transfer = await page.evaluateHandle(
      ({ name, size }) => {
        const data = new DataTransfer();
        data.items.add(new File([new Uint8Array(size)], name));
        return data;
      },
      { name, size },
    );
    await screen.dispatchEvent("drop", { dataTransfer: transfer });
  };
  const responses: number[] = [];
  page.on("response", (r) => {
    if (r.url().endsWith("/stage")) responses.push(r.status());
  });
  await drop("notes.txt", 12);
  await expect(page.locator(".terminal-status")).toContainText("Staged notes.txt");
  await drop("huge.bin", 20 * 1024 * 1024);
  await expect(page.locator(".terminal-status")).toContainText("huge.bin");
  await expect.poll(() => responses).toEqual([201, 413]);
});

test("native-only features say they need the desktop app", async ({ page }) => {
  await open(page);
  await expect(page.locator("section.desktop-settings")).toBeHidden();
  await expect(page.locator("section.connections")).toBeHidden();
  // The agent's desktop_ui tool that reads the window below reports it.
  await chat(page, "what is below");
  await expect(page.locator(".transcript .message.assistant").last()).toContainText("The webapp cannot read other windows");
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
    await open(page);
    let popups = 0;
    page.context().on("page", () => popups++);
    await page.click('button[name=pane][data-pane="preview"]');
    await page.fill("input[name=preview-url]", `http://127.0.0.1:${port}/`);
    await page.click("button[name=preview-go]");
    const frame = page.locator("section.preview iframe");
    await expect(frame).toBeVisible();
    const sandbox = (await frame.getAttribute("sandbox")) ?? "";
    expect(sandbox).toContain("allow-scripts");
    expect(sandbox).not.toContain("allow-popups");
    await expect(page.frameLocator("section.preview iframe").locator("#result")).toHaveText("popup blocked");
    expect(popups).toBe(0);
  } finally {
    site.close();
  }
});
