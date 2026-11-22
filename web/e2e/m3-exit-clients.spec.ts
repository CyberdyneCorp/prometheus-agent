// M3 exit (task 81.1): the desktop app and the dashboard attach to one
// daemon and show the same live session (spec: desktop-app, Daemon
// attachment through the bundled binary; web-dashboard, Structured chat
// over the client protocol). The desktop starts the daemon through its
// bundled binary and opens a chat; the dashboard, served by that daemon,
// opens the same session and follows it live, and a message sent from the
// dashboard appears in the desktop window without a reload. The chat's id
// is left in `$PMT_HOME/m3-exit-session` for the ACP leg of the test.
import { writeFileSync } from "node:fs";
import { expect, test } from "@playwright/test";
import { openDesktop, say } from "./desktop";
import { cli, env, signIn } from "./helpers";

test("the desktop and the dashboard show one live session", async ({ page, browser }) => {
  await openDesktop(page);
  const session = await say(page, "hello from the desktop");
  const desktop = page.locator(".transcript .message.assistant");
  await expect(desktop.last()).toContainText("mock: hello from the desktop");

  const dashboard = await (await browser.newContext()).newPage();
  const url = await signIn(dashboard);
  await dashboard.goto(`${url}chat?session=${session}`);
  const answers = dashboard.locator(".message.assistant");
  await expect(answers.last()).toContainText("mock: hello from the desktop");

  // Desktop -> dashboard, live.
  await say(page, "second word from the desktop");
  await expect(answers.last()).toContainText("mock: second word from the desktop");

  // Dashboard -> desktop, live.
  await dashboard.getByLabel("Message").fill("reply from the dashboard");
  await dashboard.getByRole("button", { name: "Send" }).click();
  await expect(desktop.last()).toContainText("mock: reply from the dashboard");
  await expect(answers.last()).toContainText("mock: reply from the dashboard");

  // One session in one daemon's store, holding all three exchanges.
  const shown = JSON.parse(cli("--json", "sessions", "show", session)) as { session: { source: string } };
  expect(shown.session.source).toBe("desktop");
  writeFileSync(`${env("PMT_HOME")}/m3-exit-session`, session);
});
