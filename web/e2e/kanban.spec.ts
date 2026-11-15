// The Kanban page (spec: web-dashboard, Kanban dashboard API and
// orchestration modes). The harness (crates/prometheus/tests/
// dashboard_browser.rs) starts a daemon whose dispatcher has claimed task
// PMT_TASK for a worker that never finishes (the mock model waits ten
// minutes) and does not tick again during the test.
import { expect, test } from "@playwright/test";
import { cli, env, signIn } from "./helpers";

function config(key: string): string {
  return cli("config", "get", key).trim();
}

test("Terminate stops a stuck worker and its task returns to ready", async ({ page }) => {
  const task = env("PMT_TASK");
  const url = await signIn(page);
  await page.goto(`${url}kanban`);
  const workers = page.locator(".kanban-workers");
  await expect(async () => {
    await page.locator("button[name=refresh]").click();
    await expect(workers).toContainText(task, { timeout: 1_000 });
  }).toPass({ timeout: 45_000 });
  await expect(page.locator(".in-progress")).toContainText("In progress: 1 of");
  await expect(page.locator(`.task-status[data-task="${task}"]`)).toHaveText("running");

  await workers.locator("button[name=details]").click();
  const details = page.locator(".run-details");
  await expect(details).toContainText(task);
  await expect(details).toContainText("running");
  await expect(details).toContainText(`${task}-r_`);

  await workers.locator("button[name=terminate]").click();
  await expect(page.locator("section.page > .status")).toContainText(`${task} is ready`);
  await expect(workers).toContainText("No worker is running.");
  await expect(page.locator(`.task-status[data-task="${task}"]`)).toHaveText("ready");
  await expect(page.locator(".kanban-inspect")).toContainText("terminated");
  const shown = JSON.parse(cli("--json", "kanban", "show", task)) as { task: { status: string } };
  expect(shown.task.status).toBe("ready");
});

test("Orchestration switches between Auto and Manual and refuses an unknown profile", async ({ page }) => {
  const url = await signIn(page);
  await page.goto(`${url}kanban`);
  const mode = page.locator(".orchestration-mode");
  await expect(mode).toHaveAttribute("data-mode", "manual");
  await page.locator("button[name=mode-auto]").click();
  await expect(mode).toHaveAttribute("data-mode", "auto");
  expect(config("kanban.auto_decompose")).toBe("true");

  const form = page.locator("form.orchestration-settings");
  await form.locator("input[name=orchestrator_profile]").fill("ghost");
  await form.locator("button[name=save]").click();
  await expect(page.locator(".orchestration-status .notice.error")).toContainText("ghost");
  expect(config("kanban.orchestrator_profile")).not.toContain("ghost");

  await form.locator("input[name=orchestrator_profile]").fill("default");
  await form.locator("button[name=save]").click();
  await expect(page.locator(".orchestration-status")).toContainText("saved");
  await expect(page.locator(".orchestration-values")).toContainText("default");
  expect(config("kanban.orchestrator_profile")).toBe("default");

  await page.locator("button[name=mode-manual]").click();
  await expect(mode).toHaveAttribute("data-mode", "manual");
  expect(config("kanban.auto_decompose")).toBe("false");
});
