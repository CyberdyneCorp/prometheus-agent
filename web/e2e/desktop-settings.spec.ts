// Settings, the palette, bot chats and profiles (spec: desktop-app, Slash
// palette from one command registry; Bot chats are created atomically by
// the daemon; Settings write validated config patches; Profile switching
// is explicit).
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { type PaletteEntry, refusal, suggest } from "../../apps/desktop/src/palette";
import { openDesktop, say, sent } from "./desktop";
import { env } from "./helpers";

test("the palette offers a runnable subcommand of an unavailable command", () => {
  const catalog: PaletteEntry[] = [
    {
      name: "skills",
      description: "skills",
      source: "builtin",
      disposition: "unavailable",
      reason: "/skills is only available in the terminal interface.",
      subcommands: [
        { name: "pending", disposition: "exec" },
        { name: "install", disposition: "unavailable", reason: "no" },
      ],
    },
    { name: "keys", description: "keys", source: "builtin", disposition: "unavailable", reason: "/keys is only available in the terminal interface." },
    { name: "deploy", description: "a skill", source: "skill", disposition: "unavailable" },
    { name: "redraw", description: "redraw", source: "builtin", disposition: "hidden" },
  ];
  expect(suggest(catalog, "/skills p")).toEqual(["/skills pending"]);
  expect(suggest(catalog, "/sk")).toEqual(["/skills"]);
  expect(suggest(catalog, "/de")).toEqual(["/deploy"]);
  expect(suggest(catalog, "/k")).toEqual([]);
  expect(suggest(catalog, "/r")).toEqual([]);
  expect(refusal(catalog, "/keys")).toBe("/keys is only available in the terminal interface.");
  expect(refusal(catalog, "/skills pending")).toBeNull();
  expect(refusal(catalog, "/skills")).toBe("/skills is only available in the terminal interface.");
});

test("the window's palette comes from the daemon's registry", async ({ page }) => {
  await openDesktop(page);
  await say(page, "hello");
  await page.fill("textarea[name=prompt]", "/skills p");
  await expect(page.locator("ul.palette li")).toHaveText(["/skills pending"]);
  await page.fill("textarea[name=prompt]", "/keys");
  await page.click("form.composer button[type=submit]");
  await expect(page.locator(".problems")).toHaveText("/keys is only available in the terminal interface.");
});

test("two windows opening the same bot during a restart share one bot chat", async ({ browser }) => {
  test.setTimeout(120_000);
  const one = await (await browser.newContext()).newPage();
  const two = await (await browser.newContext()).newPage();
  await openDesktop(one, "one");
  await openDesktop(two, "two");
  const home = env("PMT_USER_HOME");
  const restart = spawn(env("PMT_BIN"), ["--home", env("PMT_HOME"), "daemon", "restart"], { env: { ...process.env, HOME: home, USERPROFILE: home } });
  const restarted = new Promise((resolve) => restart.on("exit", resolve));
  await Promise.all([one.click("button[name=bot-chat]"), two.click("button[name=bot-chat]")]);
  await restarted;
  const chat = (page: typeof one) => page.locator("section.chat");
  await expect(chat(one)).toHaveAttribute("data-session", /^ses_/, { timeout: 60_000 });
  await expect(chat(two)).toHaveAttribute("data-session", /^ses_/, { timeout: 60_000 });
  expect(await chat(one).getAttribute("data-session")).toBe(await chat(two).getAttribute("data-session"));
  // The bot chat is not in the sessions list.
  const id = (await chat(one).getAttribute("data-session")) ?? "";
  await expect(one.locator(`nav.side [data-session="${id}"]`)).toHaveCount(0);
});

test("inherit from main agent clears the subagent route in one patch", async ({ page }) => {
  const sockets = await openDesktop(page);
  await expect(page.locator('.settings-page[data-ready="true"]')).toHaveCount(1);
  await page.selectOption("select[name=route]", "inherit");
  await page.click("form.settings-route button[type=submit]");
  await expect(page.locator("section.settings")).toContainText("Saved");
  const patches = sent(sockets, "config.patch");
  expect(patches).toHaveLength(1);
  expect(patches[0].params).toEqual({
    profile: "default",
    changes: [
      { key: "delegation.model", value: null },
      { key: "delegation.base_url", value: null },
      { key: "delegation.api_key_secret", value: null },
      { key: "delegation.wire_format", value: null },
    ],
  });
  const config = readFileSync(join(env("PMT_HOME"), "profiles/default/config.toml"), "utf8");
  expect(config).not.toMatch(/^model = "mock\/mock"/m);
  expect(config).not.toContain("base_url");
});

test("a config reload after switching names the window's profile", async ({ page }) => {
  const sockets = await openDesktop(page);
  await page.selectOption("select[name=profile]", "coder");
  await expect(page.locator(".connection")).toHaveText("Connected · coder", { timeout: 30_000 });
  await page.click("button[name=reload]");
  await expect(page.locator("section.settings")).toContainText("Configuration reloaded for coder");
  const reloads = sent(sockets, "config.reload");
  expect(reloads.at(-1)?.params).toEqual({ profile: "coder" });
});
