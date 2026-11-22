// The Bots pane (spec: bot-mode, A bot is a profile with backend metadata;
// Avatars; Roster presentation, hiding and sections; Creating,
// duplicating and deleting bots; Bot routines): driven by
// crates/prometheus/tests/desktop_e2e.rs, whose home has profiles
// `scribe` and `research` beside `default`, all served.
import { expect, type Page, test } from "@playwright/test";
import { type Cadence, scheduleOf } from "../../apps/desktop/src/schedule";
import { openDesktop } from "./desktop";

async function bots(page: Page): Promise<void> {
  await page.click('button[name=pane][data-pane="bots"]');
  await expect(page.locator(".bot-roster")).toHaveAttribute("data-ready", "true");
}

const row = (page: Page, bot: string) => page.locator(`.bot-roster li[data-bot="${bot}"]`);

async function edit(page: Page, bot: string): Promise<void> {
  await row(page, bot).locator("button[name=manage-bot]").click();
  await expect(page.locator(`form.bot-meta[data-profile="${bot}"]`)).toBeVisible();
  await expect(page.locator(".routines")).toHaveAttribute("data-ready", "true");
}

async function saveMeta(page: Page, fields: { title?: string; section?: string; hidden?: boolean }): Promise<void> {
  if (fields.title !== undefined) await page.fill("input[name=bot-title]", fields.title);
  if (fields.section !== undefined) await page.fill("input[name=bot-section]", fields.section);
  if (fields.hidden !== undefined) await page.setChecked("input[name=bot-hidden]", fields.hidden);
  await page.click("button[name=save-bot]");
}

test("the schedule picker only writes strings of the cron grammar", () => {
  const cases: [Cadence, string][] = [
    [{ kind: "daily", time: "08:00" }, "0 8 * * *"],
    [{ kind: "weekdays", time: "17:30" }, "30 17 * * 1-5"],
    [{ kind: "weekly", day: 1, time: "09:05" }, "5 9 * * 1"],
    [{ kind: "hours", every: 6 }, "every 6h"],
    [{ kind: "minutes", every: 0 }, "every 1m"],
    [{ kind: "daily", time: "nonsense" }, "0 0 * * *"],
  ];
  for (const [cadence, schedule] of cases) expect(scheduleOf(cadence)).toBe(schedule);
});

test("a title set on one desktop shows on another, with the same face", async ({ browser }) => {
  const one = await (await browser.newContext()).newPage();
  const two = await (await browser.newContext()).newPage();
  await openDesktop(one, "one");
  await openDesktop(two, "two");
  await bots(one);
  await edit(one, "research");
  await saveMeta(one, { title: "Research Buddy" });
  await expect(row(one, "research").locator(".bot-title")).toHaveText("Research Buddy");
  await bots(two);
  await expect(row(two, "research").locator(".bot-title")).toHaveText("Research Buddy");
  // Scenario: Deterministic default face.
  const face = (page: Page) => row(page, "scribe").locator("svg.avatar").evaluate((svg) => svg.outerHTML);
  expect(await face(one)).toBe(await face(two));
  await expect(row(one, "scribe").locator("svg.avatar")).toHaveAttribute("data-kind", "blob");
});

// Scenario: an uploaded avatar is what the roster shows (the daemon
// serves its picture with `bot.avatar.get`), not the monogram.
test("an uploaded avatar picture shows in the roster", async ({ page }) => {
  await openDesktop(page);
  await bots(page);
  await edit(page, "research");
  const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
  await page.setInputFiles("input[name=bot-avatar]", { name: "face.png", mimeType: "image/png", buffer: Buffer.from(png, "base64") });
  const face = row(page, "research").locator("svg.avatar");
  await expect(face).toHaveAttribute("data-kind", "image");
  await expect(face).toHaveAttribute("data-picture", "true");
  await expect(face.locator("image")).toHaveAttribute("href", `data:image/png;base64,${png}`);
  await expect(face.locator("text")).toHaveCount(0);
});

test("hiding and sections are kept on the bots and rebuilt from them", async ({ page }) => {
  await openDesktop(page);
  await bots(page);
  await edit(page, "scribe");
  await saveMeta(page, { section: "Writers" });
  await expect(page.locator('.bot-section[data-section="writers"] h3')).toContainText("Writers");
  await expect(page.locator('.bot-section[data-section="writers"] li[data-bot="scribe"]')).toHaveCount(1);
  // Deleting the section moves its bots to unassigned; the bot stays.
  await page.click('button[name=delete-section][data-section="writers"]');
  await expect(page.locator('.bot-section[data-section="writers"]')).toHaveCount(0);
  await expect(page.locator('.bot-section[data-section=""] li[data-bot="scribe"]')).toHaveCount(1);
  // Hidden: off the default roster, back with Show hidden.
  await edit(page, "scribe");
  await saveMeta(page, { hidden: true });
  await expect(row(page, "scribe")).toHaveCount(0);
  await page.check("input[name=show-hidden]");
  await expect(row(page, "scribe")).toHaveAttribute("data-hidden", "true");
  await edit(page, "scribe");
  await saveMeta(page, { hidden: false });
  await expect(row(page, "scribe")).toHaveAttribute("data-hidden", "false");
});

test("creating, duplicating and deleting a bot, and opening its Bot Chat", async ({ page }) => {
  test.setTimeout(180_000);
  await openDesktop(page);
  await bots(page);
  await page.click("button[name=new-bot]");
  await page.fill("input[name=bot-name]", "scout");
  await page.fill("input[name=bot-new-title]", "Scout");
  await page.click("button[name=create-bot]");
  await expect(page.locator('.bot-created[data-profile="scout"]')).toBeVisible({ timeout: 150_000 });
  await expect(row(page, "scout").locator(".bot-title")).toHaveText("Scout");
  await edit(page, "scout");
  await page.fill("input[name=duplicate-name]", "scout-2");
  await page.click("button[name=duplicate-bot]");
  await expect(row(page, "scout-2").locator(".bot-title")).toHaveText("Scout");
  await edit(page, "scout-2");
  await page.click("button[name=delete-bot]");
  await page.click(".confirm button[name=confirm]");
  await expect(row(page, "scout-2")).toHaveCount(0);
  // The default profile cannot be deleted from the pane.
  await edit(page, "default");
  await expect(page.locator("button[name=delete-bot]")).toBeDisabled();
  // Opening a bot opens its canonical Bot Chat, kept out of the sessions list.
  await row(page, "scout").locator("button[name=open-bot]").click();
  const chat = page.locator("section.chat");
  await expect(chat).toHaveAttribute("data-session", /^ses_/, { timeout: 60_000 });
  const id = (await chat.getAttribute("data-session")) ?? "";
  await expect(page.locator(`nav.side [data-session="${id}"]`)).toHaveCount(0);
  // Scenario: /new keeps the relationship: the Bot Chat stays open.
  await page.fill("textarea[name=prompt]", "/new");
  await page.click("form.composer button[type=submit]");
  await expect(page.locator(".problems")).not.toBeEmpty();
  await expect(chat).toHaveAttribute("data-session", id);
});

test("routines are scheduled jobs delivered into the Bot Chat", async ({ page }) => {
  await openDesktop(page);
  await bots(page);
  await edit(page, "scribe");
  const add = async (name: string, pick: (p: Page) => Promise<void>) => {
    await page.fill("input[name=routine-name]", name);
    await page.fill("textarea[name=routine-prompt]", `Do the ${name}.`);
    await pick(page);
    await page.click("button[name=add-routine]");
    await expect(page.locator(`.routine-list li[data-routine="${name}"]`)).toHaveCount(1);
  };
  await add("morning digest", async (p) => {
    await p.selectOption("select[name=routine-cadence]", "daily");
    await p.fill("input[name=routine-time]", "08:00");
  });
  await add("standup", async (p) => {
    await p.selectOption("select[name=routine-cadence]", "weekdays");
    await p.fill("input[name=routine-time]", "09:15");
  });
  await add("weekly review", async (p) => {
    await p.selectOption("select[name=routine-cadence]", "weekly");
    await p.selectOption("select[name=routine-day]", "5");
    await p.fill("input[name=routine-time]", "16:00");
  });
  await add("inbox sweep", async (p) => {
    await p.selectOption("select[name=routine-cadence]", "hours");
    await p.fill("input[name=routine-every]", "3");
  });
  await add("pulse", async (p) => {
    await p.selectOption("select[name=routine-cadence]", "minutes");
    await p.fill("input[name=routine-every]", "45");
  });
  const routine = (name: string) => page.locator(`.routine-list li[data-routine="${name}"]`);
  await expect(routine("morning digest")).toContainText("0 8 * * *");
  await expect(routine("standup")).toContainText("15 9 * * 1-5");
  await expect(routine("weekly review")).toContainText("0 16 * * 5");
  await expect(routine("inbox sweep")).toContainText("every 3h");
  await expect(routine("pulse")).toContainText("every 45m");
  await routine("standup").locator("button[name=pause-routine]").click();
  await expect(routine("standup")).toHaveAttribute("data-state", "paused");
  await routine("pulse").locator("button[name=remove-routine]").click();
  await expect(routine("pulse")).toHaveCount(0);
});
