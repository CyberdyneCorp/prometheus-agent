// Host-only speech providers and missing ones (spec: voice-mode, Client-
// direct voice for remote daemons; Desktop voice controls), driven by
// crates/prometheus/tests/desktop_voice.rs: `default` transcribes with a
// command provider that runs on the daemon host, so the recording is
// relayed through `voice.transcribe`; `mute` resolves no provider, so its
// voice controls are hidden.
import { expect, test } from "@playwright/test";
import { openDesktop, say, sent } from "./desktop";
import { fakeMicrophone, voiceBrowser } from "./voice";

test.use(voiceBrowser());

test("a host-only provider gets the recording through the daemon", async ({ page }) => {
  await fakeMicrophone(page);
  const sockets = await openDesktop(page);
  await page.click("button[name=dictate]");
  await expect(page.locator("textarea[name=prompt]")).toHaveValue("relayed words", { timeout: 30_000 });
  const relayed = sent(sockets, "voice.transcribe");
  expect(relayed).toHaveLength(1);
  const params = relayed[0]?.params as { profile: string; audio: string };
  expect(params.profile).toBe("default");
  // A WAV recording, base64.
  expect(Buffer.from(params.audio, "base64").subarray(0, 4).toString()).toBe("RIFF");
});

test("controls whose provider does not resolve are hidden", async ({ page }) => {
  await fakeMicrophone(page);
  await openDesktop(page);
  await expect(page.locator("button[name=dictate]")).toBeVisible();
  await page.selectOption("select[name=profile]", "mute");
  await expect(page.locator(".connection")).toHaveText(/Connected · mute/, { timeout: 60_000 });
  for (const name of ["dictate", "voice-chat", "wake-ear"]) await expect(page.locator(`button[name=${name}]`)).toBeHidden();
  await say(page, "hi");
  const answer = page.locator(".transcript .message.assistant").last();
  await expect(answer).toContainText("mock: hi");
  await expect(answer.locator("button[name=read-aloud]")).toBeHidden();
});
