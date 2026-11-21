// The desktop's voice controls with cloud speech providers (spec: voice-
// mode, Desktop voice controls; Client-direct voice for remote daemons;
// Barge-in), driven by crates/prometheus/tests/desktop_voice.rs: OpenAI
// speech-to-text and text-to-speech hold a key, so the page calls them
// itself and the daemon only ever gets text.
import { expect, test } from "@playwright/test";
import { openDesktop, say, sent } from "./desktop";
import { env } from "./helpers";
import { fakeMicrophone, voiceBrowser, openAiSpeech, received, silence } from "./voice";

test.use(voiceBrowser());

const INTERRUPTED = "[Your previous spoken reply was cut off: the user started talking over it.]";

test("read aloud speaks an assistant message and stops when clicked again", async ({ page }) => {
  await fakeMicrophone(page);
  const heard = await openAiSpeech(page, ["unused"], silence(8));
  await openDesktop(page);
  await say(page, "hello there");
  const answer = page.locator(".transcript .message.assistant").last();
  await expect(answer).toContainText("mock: hello there");
  const read = answer.locator("button[name=read-aloud]");
  await expect(read).toBeVisible();
  await read.click();
  await expect(read).toHaveAttribute("aria-pressed", "true");
  await expect.poll(() => heard.tts.length).toBe(1);
  const request = JSON.parse(heard.tts[0]?.body ?? "{}") as { input?: string; model?: string };
  expect(request.input).toBe("mock: hello there");
  expect(heard.tts[0]?.authorization).toBe(`Bearer ${env("PMT_SPEECH_KEY")}`);
  // Still playing (the reply lasts eight seconds) until clicked again.
  await expect(read).toHaveAttribute("aria-pressed", "true");
  await read.click();
  await expect(read).toHaveAttribute("aria-pressed", "false");
});

test("dictation fills the composer from the provider the page calls itself", async ({ page }) => {
  await fakeMicrophone(page);
  const heard = await openAiSpeech(page, ["dictated words"], silence(1));
  const sockets = await openDesktop(page);
  await page.click("button[name=dictate]");
  await expect(page.locator(".voice-pill")).toHaveText("Listening…");
  await expect(page.locator("textarea[name=prompt]")).toHaveValue("dictated words", { timeout: 30_000 });
  expect(heard.stt).toHaveLength(1);
  expect(heard.stt[0]?.authorization).toBe(`Bearer ${env("PMT_SPEECH_KEY")}`);
  expect(heard.stt[0]?.body).toContain("RIFF");
  // Nothing was sent: dictation only fills the box.
  expect(sent(sockets, "prompt.submit")).toHaveLength(0);
  expect(sent(sockets, "voice.transcribe")).toHaveLength(0);
});

test("a voice conversation sends only text to the daemon and talking over the reply interrupts it", async ({ page }) => {
  await fakeMicrophone(page);
  const frames = received(page);
  const heard = await openAiSpeech(page, ["what is on my calendar", "never mind that, what time is it"], silence(12));
  const sockets = await openDesktop(page);
  const talk = page.locator("button[name=voice-chat]");
  await talk.click();
  await expect(talk).toHaveAttribute("aria-pressed", "true");
  // The first utterance is asked, answered and spoken.
  await expect.poll(() => sent(sockets, "prompt.submit").length, { timeout: 30_000 }).toBeGreaterThanOrEqual(1);
  await expect(page.locator(".transcript .message.assistant").first()).toContainText("mock: what is on my calendar");
  await expect.poll(() => heard.tts.length, { timeout: 30_000 }).toBeGreaterThanOrEqual(1);
  // The microphone's next utterance talks over the twelve-second reply:
  // playback stops and the next message carries the note.
  await expect.poll(() => sent(sockets, "prompt.submit").length, { timeout: 30_000 }).toBeGreaterThanOrEqual(2);
  const prompts = sent(sockets, "prompt.submit").map((f) => f.params as { text: string; voice?: boolean; files: unknown[] });
  expect(prompts[0]).toMatchObject({ text: "what is on my calendar", voice: true });
  expect(prompts[1]?.text).toBe(`${INTERRUPTED}\nnever mind that, what time is it`);
  await talk.click();
  await expect(talk).toHaveAttribute("aria-pressed", "false");
  await expect(page.locator("section.chat")).toHaveAttribute("data-voice", "idle");
  // Audio never crossed the daemon link: no relay calls, and nothing the
  // page sent or received carries audio.
  for (const method of ["voice.transcribe", "voice.speak"]) expect(sent(sockets, method)).toHaveLength(0);
  expect(heard.stt.length).toBeGreaterThanOrEqual(2);
  for (const frame of [...sockets.flat(), ...frames]) expect(frame).not.toMatch(/"(audio|pcm)":/);
});
