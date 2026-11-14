// The git review pane, simple mode's thinking and inline diffs, the
// memory graph and HUD mode (spec: desktop-app, Git review pane;
// Interface modes and layouts; Memory graph; HUD mode), driven by
// crates/prometheus/tests/desktop_review.rs.
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";
import { emit, nativeCalls, openDesktop, say, sent } from "./desktop";
import { env } from "./helpers";

const answers = (page: Page) => page.locator(".transcript .message.assistant");

async function inRepo(page: Page, text: string): Promise<string> {
  await emit(page, "deep-link", { open: "new", cwd: env("PMT_REPO") });
  const id = await say(page, text);
  await expect(answers(page).last()).toHaveText(/ok/, { timeout: 60_000 });
  return id;
}

test("last-turn shows the turn's edits and commit read-only; revert asks first", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  await inRepo(page, "edit two files");
  await page.keyboard.press("Control+g");
  const pane = page.locator("section.review");
  await expect(pane).toBeVisible();
  await page.selectOption("select[name=review-scope]", "last-turn");
  await expect(pane).toHaveAttribute("data-scope", "last-turn");
  await expect(pane.locator(".review-files li")).toHaveCount(2);
  await expect(pane.locator('li[data-path="one.txt"]')).toBeVisible();
  await expect(pane.locator('li[data-path="two.txt"]')).toBeVisible();
  await expect(pane.locator("button[name=stage], button[name=unstage], button[name=revert], button[name=commit]")).toHaveCount(0);
  // Uncommitted changes can be reverted, after a confirmation.
  await page.selectOption("select[name=review-scope]", "uncommitted");
  await expect(pane).toHaveAttribute("data-scope", "uncommitted");
  const two = pane.locator('li[data-path="two.txt"]');
  await two.locator("button[name=revert]").click();
  const confirm = page.locator(".confirm");
  await expect(confirm).toContainText("two.txt");
  await confirm.locator("button[name=cancel]").click();
  await expect(two).toBeVisible();
  await two.locator("button[name=revert]").click();
  await page.locator(".confirm button[name=confirm]").click();
  await expect(pane.locator('li[data-path="two.txt"]')).toHaveCount(0);
});

test("simple mode collapses thinking and hides inline diffs", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  await inRepo(page, "patch the readme");
  await expect(page.locator("details.thinking")).toHaveAttribute("open", "");
  await expect(page.locator("pre.inline-diff")).toContainText("hello world");
  await page.selectOption("select[name=interface-mode]", "simple");
  await expect(page.locator("pre.inline-diff")).toBeHidden();
  await expect(page.locator(".transcript .tool")).toBeHidden();
  await say(page, "patch the readme again");
  await expect(answers(page).last()).toHaveText(/ok/, { timeout: 60_000 });
  await expect(page.locator("details.thinking").last()).not.toHaveAttribute("open", "");
  await page.selectOption("select[name=interface-mode]", "advanced");
});

test("the share code is content-free and a deleted memory leaves open chats alone", async ({ page }) => {
  test.setTimeout(120_000);
  await openDesktop(page);
  const before = await say(page, "remember me");
  await expect(answers(page).last()).toHaveText(/ok/, { timeout: 60_000 });
  await page.fill("textarea[name=prompt]", "/journey");
  await page.click("form.composer button[type=submit]");
  const graph = page.locator("section.memory-graph");
  await expect(graph).toHaveAttribute("data-ready", "true");
  const node = graph.locator("circle.node.memory");
  await expect(node).toHaveCount(1);
  await page.click("button[name=share-graph]");
  const code = await page.locator("input[name=share-code]").inputValue();
  const layout = JSON.parse(Buffer.from(code.replace(/^pmg1\./, ""), "base64").toString("utf8")) as { nodes: Record<string, unknown>[] };
  expect(layout.nodes.length).toBeGreaterThan(0);
  for (const n of layout.nodes) expect(Object.keys(n).sort()).toEqual(["id", "x", "y"]);
  expect(code).not.toContain("saffron");
  expect(Buffer.from(code.slice(5), "base64").toString("utf8")).not.toContain("saffron");
  await node.click();
  await page.click("button[name=delete-node]");
  await expect(graph.locator("circle.node.memory")).toHaveCount(0);
  await page.click("button[name=close-graph]");
  // The open chat keeps its snapshot; a new session does not have the memory.
  await say(page, "still there");
  await expect(answers(page).last()).toHaveText(/ok/, { timeout: 60_000 });
  await page.click("button[name=new-chat]");
  const after = await say(page, "fresh start");
  await expect(answers(page).last()).toHaveText(/ok/, { timeout: 60_000 });
  writeFileSync(join(env("PMT_HOME"), "graph-sessions.json"), JSON.stringify({ before, after }));
});

test("a prompt from the HUD carries the window underneath, never pixels", async ({ browser }) => {
  test.setTimeout(120_000);
  const main = await (await browser.newContext()).newPage();
  await openDesktop(main);
  const id = await say(main, "hello");
  await expect(answers(main).last()).toHaveText(/ok/, { timeout: 60_000 });
  await main.keyboard.press("Control+Shift+H");
  await expect.poll(async () => await nativeCalls()).toContain(`hud #hud=default/${id}`);
  const hud = await (await browser.newContext()).newPage();
  const sockets = await openDesktop(hud, "hud", `#hud=default/${id}`);
  await expect(hud.locator("section.chat")).toHaveAttribute("data-session", id);
  await expect(hud.locator("button[name=hud-reset]")).toBeVisible();
  await hud.fill("textarea[name=prompt]", "summarise this");
  await hud.click("form.composer button[type=submit]");
  await expect.poll(() => sent(sockets, "prompt.submit").length).toBeGreaterThan(0);
  const submitted = sent(sockets, "prompt.submit").at(-1)?.params as { text: string; images?: unknown[] };
  expect(submitted.text).toContain("summarise this");
  expect(submitted.text).toContain("Invoice 42");
  expect(submitted.text).toContain("Firefox");
  expect(submitted.images ?? []).toEqual([]);
});

test("on native Wayland snapping is disabled with the compositor's reason", async ({ page }) => {
  await openDesktop(page, "main", "", env("PMT_DESKTOP_WAYLAND"));
  const snap = page.locator('input[name="desktop.hud.snap_hotkey"]');
  await expect(snap).toBeDisabled();
  await expect(page.locator('.reason[data-for="desktop.hud.snap_hotkey"]')).toHaveText("the compositor controls window placement");
});
