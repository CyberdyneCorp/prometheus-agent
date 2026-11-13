// The desktop chat (spec: desktop-app, Chat transcript rendering; Drafts
// are durable and never sent for the user; Composer history and queue
// editing; Desktop status bar; Composer model picker; Subagent route
// settings are drafts until applied), driven by crates/prometheus/tests/
// desktop_chat.rs against the real daemon with a scripted mock model.
import { spawn, spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { nativeCalls, openDesktop, say, sent } from "./desktop";
import { env } from "./helpers";

const answers = (page: Page) => page.locator(".transcript .message.assistant");
const config = () => readFileSync(join(env("PMT_HOME"), "profiles/default/config.toml"), "utf8");

/** Runs the CLI on the test home (as the user would in a terminal). */
function cli(args: string[], input?: string): void {
  const home = env("PMT_USER_HOME");
  const out = spawnSync(env("PMT_BIN"), ["--home", env("PMT_HOME"), ...args], { input, env: { ...process.env, HOME: home, USERPROFILE: home } });
  expect(out.status, `${args.join(" ")}: ${String(out.stderr)}`).toBe(0);
}

async function type(page: Page, text: string): Promise<void> {
  await page.fill("textarea[name=prompt]", text);
  await page.click("form.composer button[type=submit]");
}

test("the timeline jumps to a prompt and Show earlier pages backwards", async ({ page }) => {
  test.setTimeout(300_000);
  await openDesktop(page);
  const id = await say(page, "prompt 1");
  for (let i = 2; i <= 30; i++) {
    await expect(answers(page)).toHaveCount(i - 1);
    await type(page, `prompt ${i}`);
  }
  await expect(answers(page)).toHaveCount(30);
  await page.reload();
  await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 60_000 });
  await page.click(`nav.side [data-session="${id}"]`);
  await expect(page.locator("nav.timeline .marker")).toHaveCount(30);
  await expect(page.locator('[data-prompt="30"]')).toHaveCount(1);
  await expect(page.locator('[data-prompt="20"]')).toHaveCount(0);
  await page.click('nav.timeline [data-marker="5"]');
  await expect(page.locator('[data-prompt="5"]')).toBeInViewport();
  await expect(page.locator('[data-prompt="4"]')).toHaveCount(0);
  await page.click("button[name=show-earlier]");
  await expect(page.locator('[data-prompt="4"]')).toHaveCount(1);
  await expect(page.locator('[data-prompt="1"]')).toHaveCount(1);
});

test("a tool's image is read through the session's connection, never opened locally", async ({ page }) => {
  const sockets = await openDesktop(page);
  const id = await say(page, "draw the chart");
  await expect(answers(page).last()).toHaveText(/Drew it/);
  const row = page.locator('.tool[data-tool="write_file"]');
  await row.locator("details.tool-detail summary").click();
  await expect(row.locator("img")).toHaveAttribute("src", /^data:image\/svg\+xml;base64,/);
  expect(sent(sockets, "fs.read").at(-1)?.params).toEqual({ session_id: id, path: "out/chart.svg" });
  expect((await nativeCalls()).filter((c) => c.startsWith("open_path"))).toEqual([]);
});

test("a draft of a session deleted elsewhere comes back in a fresh draft and is not sent", async ({ page }) => {
  const sockets = await openDesktop(page);
  const id = await say(page, "keep me");
  await expect(answers(page)).toHaveCount(1);
  await page.fill("textarea[name=prompt]", "my unsent words");
  await page.reload();
  await expect(page.locator(".connection")).toHaveText(/Connected/, { timeout: 60_000 });
  const submitted = sent(sockets, "prompt.submit").length;
  cli(["sessions", "delete", id, "--yes"]);
  await page.click(`nav.side [data-session="${id}"]`);
  await expect(page.locator(".restore-strip")).toContainText("Restored your unsent message");
  await expect(page.locator("textarea[name=prompt]")).toHaveValue("my unsent words");
  await expect(page.locator("section.chat")).toHaveAttribute("data-session", "");
  await page.waitForTimeout(500);
  expect(sent(sockets, "prompt.submit")).toHaveLength(submitted);
  await page.click("button[name=undo-restore]");
  await expect(page.locator("textarea[name=prompt]")).toHaveValue("");
});

test("a queued prompt edited in one window shows edited in another", async ({ browser }) => {
  test.setTimeout(120_000);
  const one = await (await browser.newContext()).newPage();
  await openDesktop(one, "one");
  const id = await say(one, "slow");
  await type(one, "second");
  await type(one, "third");
  await expect(one.locator("section.queue .queue-entry")).toHaveCount(2);
  const two = await (await browser.newContext()).newPage();
  await openDesktop(two, "two");
  await two.click(`nav.side [data-session="${id}"]`);
  await expect(two.locator("section.queue .queue-entry")).toHaveCount(2);
  const third = one.locator("section.queue .queue-entry", { hasText: "third" });
  await third.locator("button[name=edit]").click();
  await one.fill("input[name=queue-text]", "third, edited");
  await one.click("section.queue button[name=save]");
  await expect(two.locator("section.queue .queue-entry").nth(1)).toContainText("third, edited");
  // Stop interrupts the slow turn and pauses the queue.
  await one.click("form.composer button[name=stop]");
  await expect(one.locator(".queue-paused")).toBeVisible();
  await expect(two.locator(".queue-paused")).toBeVisible();
});

test("a turn lost in a daemon restart blocks the queue until it is discarded", async ({ page }) => {
  test.setTimeout(180_000);
  await openDesktop(page);
  await say(page, "slow");
  await type(page, "after the restart");
  await expect(page.locator("section.queue .queue-entry")).toHaveCount(1);
  const home = env("PMT_USER_HOME");
  const restart = spawn(env("PMT_BIN"), ["--home", env("PMT_HOME"), "daemon", "restart"], { env: { ...process.env, HOME: home, USERPROFILE: home } });
  await new Promise((resolve) => restart.on("exit", resolve));
  const lost = page.locator("section.queue .queue-entry.lost");
  await expect(lost).toContainText("Turn lost during restart", { timeout: 60_000 });
  await expect(lost.locator("button")).toHaveText(["Discard"]);
  const queued = page.locator("section.queue .queue-entry:not(.lost)");
  await expect(queued).toContainText("after the restart");
  // Starting would take it off the queue: it stays while the lost turn
  // blocks it.
  await page.waitForTimeout(3_000);
  await expect(queued).toHaveCount(1);
  await expect(page.locator(".transcript .message.assistant .text").filter({ hasText: /^ok$/ })).toHaveCount(0);
  await lost.locator("button[name=discard]").click();
  await expect(page.locator("section.queue .queue-entry")).toHaveCount(0, { timeout: 30_000 });
  await expect(page.locator(".transcript .message.assistant .text").filter({ hasText: /^ok$/ })).toHaveCount(1, { timeout: 30_000 });
});

test("the context meter's categories sum to its count", async ({ page }) => {
  await openDesktop(page);
  await say(page, "hello");
  await expect(answers(page)).toHaveCount(1);
  const meter = page.locator("button[name=context-meter]");
  await expect(meter).toHaveAttribute("data-tokens", /^[1-9]\d*$/);
  await meter.click();
  const rows = page.locator(".context-popover li");
  await expect(rows).toHaveCount(8);
  // Read in one go: a turn's end refreshes the meter and its popover together.
  const shown = await page.evaluate(() => ({
    meter: Number(document.querySelector("button[name=context-meter]")?.getAttribute("data-tokens")),
    rows: Array.from(document.querySelectorAll(".context-popover li")).map((e) => [e.getAttribute("data-category"), Number(e.getAttribute("data-tokens"))] as const),
  }));
  expect(shown.rows.map(([c]) => c)).toEqual(["system_prompt", "tool_definitions", "skills", "memory", "rules", "mcp", "subagent_definitions", "conversation"]);
  expect(shown.rows.reduce((total, [, tokens]) => total + tokens, 0)).toBe(shown.meter);
});

test("approval bypass lasts for its session only", async ({ page }) => {
  await openDesktop(page);
  await say(page, "in session a");
  // `/yolo` waits for no turn: the switch is used once the answer is in.
  await expect(answers(page)).toHaveCount(1);
  await page.check("input[name=bypass]");
  await page.click(".confirm button[name=confirm]");
  await expect(page.locator("input[name=bypass]")).toBeChecked();
  await expect(page.locator("input[name=bypass]")).toHaveAttribute("data-applied", "true");
  await type(page, "/approvals");
  await expect(page.locator(".problems")).toContainText("YOLO");
  await page.click("button[name=new-chat]");
  await say(page, "in session b");
  await expect(page.locator("input[name=bypass]")).not.toBeChecked();
  await type(page, "/approvals");
  await expect(page.locator(".problems")).not.toContainText("YOLO");
  await expect(page.locator(".problems")).toContainText("approvals");
});

test("a bypass switched on while a turn runs is refused and the switch goes back", async ({ page }) => {
  await openDesktop(page);
  await say(page, "slow");
  await page.check("input[name=bypass]");
  await page.click(".confirm button[name=confirm]");
  const bypass = page.locator("input[name=bypass]");
  await expect(bypass).toHaveAttribute("data-applied", "false");
  await expect(bypass).not.toBeChecked();
  await expect(page.locator(".bypass-note")).toContainText("/yolo is unavailable while the agent is working");
  await page.click("button[name=stop]");
});

test("a model picked for new chats leaves the profile's default alone", async ({ page }) => {
  const sockets = await openDesktop(page);
  await page.click("details.model-picker summary");
  await page.locator('.model-group:not([data-group="Favourites"]) button[name=pick-model][data-model="mock/model-b"]').click();
  await say(page, "hello b");
  expect((sent(sockets, "session.create").at(-1)?.params as { setup: unknown }).setup).toEqual({ model: "mock/model-b" });
  await expect(page.locator("details.model-picker .model-name")).toHaveText("mock/model-b");
  expect(config()).toMatch(/^default = "model-a"$/m);
});

test("a starred model whose provider signs out comes back at its place", async ({ page }) => {
  cli(["secrets", "set", "DEEPSEEK_API_KEY"], "sk-test-deepseek-0000000000");
  await openDesktop(page);
  const star = (id: string) => page.locator(`.model-group:not([data-group="Favourites"]) button[name=star][data-model="${id}"]`).click();
  const favourites = () => page.locator('.model-group[data-group="Favourites"] li');
  const reopen = async () => {
    await page.click("button[name=new-chat]");
    if (!(await page.locator("details.model-picker").evaluate((d) => (d as HTMLDetailsElement).open))) await page.click("details.model-picker summary");
  };
  await reopen();
  for (const id of ["mock/model-b", "deepseek/deepseek-chat", "mock/model-a"]) await star(id);
  await expect(favourites()).toHaveCount(3);
  expect(await favourites().evaluateAll((els) => els.map((e) => e.getAttribute("data-model")))).toEqual(["mock/model-b", "deepseek/deepseek-chat", "mock/model-a"]);
  cli(["secrets", "rm", "DEEPSEEK_API_KEY"]);
  await reopen();
  await expect(favourites()).toHaveCount(2);
  cli(["secrets", "set", "DEEPSEEK_API_KEY"], "sk-test-deepseek-0000000000");
  await reopen();
  await expect(favourites()).toHaveCount(3);
  expect(await favourites().evaluateAll((els) => els.map((e) => e.getAttribute("data-model")))).toEqual(["mock/model-b", "deepseek/deepseek-chat", "mock/model-a"]);
});

test("an unapplied subagent route is discarded when the page is left", async ({ page }) => {
  const sockets = await openDesktop(page);
  const before = config();
  const patches = sent(sockets, "config.patch").length;
  const ready = (name: string) => expect(page.locator(`.settings-page[data-page="${name}"][data-ready="true"]`)).toHaveCount(1);
  await page.click('button[name=settings-page][data-page="subagents"]');
  await ready("subagents");
  await page.selectOption("select[name=provider]", "mock");
  await page.click("form.settings-route button[type=submit]");
  await expect(page.locator("form.settings-route .field-error")).toHaveText("Choose a model for this provider, or Use parent model.");
  await page.click('button[name=settings-page][data-page="general"]');
  await ready("general");
  await page.click('button[name=settings-page][data-page="subagents"]');
  await ready("subagents");
  await expect(page.locator("select[name=provider]")).toHaveValue("");
  expect(sent(sockets, "config.patch")).toHaveLength(patches);
  expect(config()).toBe(before);
});
