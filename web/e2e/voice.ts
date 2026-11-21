// The voice specs' stand-ins (crates/prometheus/tests/desktop_voice.rs):
// the page's microphone plays the harness's recording in a loop, and
// the cloud speech providers the page calls directly are answered here at
// their real URLs, recording what reached them.
import { readFileSync } from "node:fs";
import type { Page, Route } from "@playwright/test";
import { env } from "./helpers";

/** Launch options: audio plays without a gesture, and the webview
 *  origin resolves to the harness for the one request Playwright cannot
 *  route (the microphone's AudioWorklet module). */
export function voiceBrowser(): { launchOptions: { args: string[] } } {
  const harness = new URL(env("PMT_DESKTOP_HARNESS"));
  return { launchOptions: { args: ["--autoplay-policy=no-user-gesture-required", `--host-resolver-rules=MAP tauri.localhost ${harness.host}`] } };
}

/** The microphone: `getUserMedia` answers with the harness's recording
 *  played in a loop (one second of speech-level tone, three of silence),
 *  the same on every OS the suite runs on. */
export async function fakeMicrophone(page: Page): Promise<void> {
  const wav = readFileSync(env("PMT_FAKE_MIC")).toString("base64");
  await page.addInitScript((b64: string) => {
    navigator.mediaDevices.getUserMedia = async () => {
      const context = new AudioContext();
      const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
      const source = context.createBufferSource();
      source.buffer = await context.decodeAudioData(bytes.buffer);
      source.loop = true;
      const out = context.createMediaStreamDestination();
      source.connect(out);
      source.start();
      return out.stream;
    };
  }, wav);
}

/** One request a speech provider received. */
export interface Heard {
  url: string;
  authorization: string;
  body: string;
}

/** `seconds` of 16 kHz silence as a WAV file: a reply that plays long
 *  enough to be talked over or stopped. */
export function silence(seconds: number): Buffer {
  const samples = 16_000 * seconds;
  const out = Buffer.alloc(44 + samples * 2);
  out.write("RIFF", 0);
  out.writeUInt32LE(36 + samples * 2, 4);
  out.write("WAVEfmt ", 8);
  out.writeUInt32LE(16, 16);
  out.writeUInt16LE(1, 20);
  out.writeUInt16LE(1, 22);
  out.writeUInt32LE(16_000, 24);
  out.writeUInt32LE(32_000, 28);
  out.writeUInt16LE(2, 32);
  out.writeUInt16LE(16, 34);
  out.write("data", 36);
  out.writeUInt32LE(samples * 2, 40);
  return out;
}

const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-headers": "authorization, content-type, xi-api-key, x-goog-api-key",
  "access-control-allow-methods": "POST, OPTIONS",
};

/** Answers OpenAI's transcription endpoint with `transcripts` in turn
 *  (the last one repeats) and its speech endpoint with `speech`; resolves
 *  the requests each received as they arrive. */
export async function openAiSpeech(page: Page, transcripts: string[], speech: Buffer): Promise<{ stt: Heard[]; tts: Heard[] }> {
  const heard = { stt: [] as Heard[], tts: [] as Heard[] };
  const answer = async (route: Route, into: Heard[], reply: () => Promise<void>) => {
    const request = route.request();
    if (request.method() === "OPTIONS") return route.fulfill({ status: 204, headers: CORS });
    into.push({ url: request.url(), authorization: (await request.allHeaders()).authorization ?? "", body: request.postDataBuffer()?.toString("latin1") ?? "" });
    return reply();
  };
  await page.route("https://api.openai.com/v1/audio/transcriptions", (route) =>
    answer(route, heard.stt, () => {
      const text = transcripts[Math.min(heard.stt.length - 1, transcripts.length - 1)] ?? "";
      return route.fulfill({ status: 200, headers: CORS, contentType: "application/json", body: JSON.stringify({ text }) });
    }),
  );
  await page.route("https://api.openai.com/v1/audio/speech", (route) =>
    answer(route, heard.tts, () => route.fulfill({ status: 200, headers: CORS, contentType: "audio/wav", body: speech })),
  );
  return heard;
}

/** Every frame the page received on its sockets (`openDesktop` records
 *  only what it sent). */
export function received(page: Page): string[] {
  const frames: string[] = [];
  page.on("websocket", (ws) => ws.on("framereceived", (frame) => frames.push(String(frame.payload))));
  return frames;
}
