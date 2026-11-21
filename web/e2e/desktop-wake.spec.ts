// The desktop's wake ear (spec: voice-mode, Wake word profile routing;
// Wake capture location for remote clients; Desktop voice controls),
// driven by crates/prometheus/tests/desktop_voice.rs: the ear streams the
// fake microphone to the daemon's listener (`wake.feed`); its stand-in
// engine hears `coder`'s phrase, and the window switches to `coder` and
// answers the next utterance in a new voice session there.
import { expect, test } from "@playwright/test";
import { openDesktop, sent } from "./desktop";
import { fakeMicrophone, voiceBrowser, openAiSpeech, silence } from "./voice";

test.use(voiceBrowser());

test("another profile's wake phrase switches the window to it", async ({ page }) => {
  await fakeMicrophone(page);
  const heard = await openAiSpeech(page, ["what is next on my list"], silence(1));
  const sockets = await openDesktop(page);
  const ear = page.locator(".wake");
  await page.click("button[name=wake-ear]");
  // Solid while armed, on the daemon with the desktop's microphone.
  await expect(ear).toHaveAttribute("data-state", /armed|busy/, { timeout: 30_000 });
  const armed = sent(sockets, "wake.arm")[0]?.params;
  expect(armed).toEqual({ profile: "default", client_capture: true });
  await expect.poll(() => sent(sockets, "wake.feed").length, { timeout: 30_000 }).toBeGreaterThan(0);
  // "hey coder": the window shows `coder` and a new voice session there.
  await expect(page.locator(".connection")).toHaveText(/Connected · coder/, { timeout: 60_000 });
  await expect(page.locator("select[name=profile]")).toHaveValue("coder");
  await expect.poll(() => sent(sockets, "prompt.submit").length, { timeout: 60_000 }).toBe(1);
  const created = sent(sockets, "session.create").map((f) => f.params as { profile: string });
  expect(created.at(-1)?.profile).toBe("coder");
  const asked = sent(sockets, "prompt.submit")[0]?.params as { text: string; voice: boolean };
  expect(asked).toMatchObject({ text: "what is next on my list", voice: true });
  await expect(page.locator(".transcript .message.assistant").last()).toContainText("mock: what is next on my list");
  expect(heard.stt.length).toBeGreaterThanOrEqual(1);
  // Listening resumes, now on `coder`.
  await expect.poll(() => sent(sockets, "wake.arm").map((f) => (f.params as { profile: string }).profile), { timeout: 30_000 }).toContain("coder");
  await expect(ear).toHaveAttribute("data-state", /armed|busy/);
});
