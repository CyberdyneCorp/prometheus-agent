// The profile's speech providers as the desktop reaches them (spec:
// voice-mode, Client-direct voice for remote daemons): `voice.client_config`
// says per provider whether the page calls it directly (cloud providers
// with an API key: recordings never cross the daemon link, only text goes
// to the daemon) or relays through the daemon (`voice.transcribe`,
// `voice.speak`: host-only or signed-in providers, or
// `voice.client_direct = false`). The configuration, and with it the key,
// is fetched for one voice action and dropped when it ends.

import type { ClientSpeech, VoiceClientConfig, VoiceSpoken, VoiceTranscript } from "../../../../schema/rpc";
import { base64, unbase64, wav } from "./mic";

/** What the voice code needs of the daemon connection. */
export interface VoiceLink {
  readonly profile: string;
  call<T>(method: string, params: unknown): Promise<T>;
}

/** The voice settings of `link`'s profile; `credentials` only right
 *  before a direct provider is called (the key lives for that action). */
export function voiceConfig(link: VoiceLink, credentials: boolean): Promise<VoiceClientConfig> {
  return link.call<VoiceClientConfig>("voice.client_config", { profile: link.profile, credentials });
}

/** What a speech provider answered with an error status. */
async function failure(res: Response): Promise<Error> {
  const body = await res.text().catch(() => "");
  let message = body.slice(0, 200);
  try {
    const parsed = JSON.parse(body) as { error?: { message?: string } | string; detail?: { message?: string } | string };
    const error = parsed.error ?? parsed.detail;
    message = (typeof error === "string" ? error : error?.message) ?? message;
  } catch {
    // Not JSON: the start of the body says enough.
  }
  return new Error(`HTTP ${res.status}${message ? `: ${message}` : ""}`);
}

async function ok(res: Promise<Response>): Promise<Response> {
  const answered = await res;
  if (!answered.ok) throw await failure(answered);
  return answered;
}

/** Transcribes 16 kHz `samples` with the profile's STT provider. */
export async function transcribe(link: VoiceLink, stt: ClientSpeech, samples: Int16Array, sessionId: string | null): Promise<string> {
  const audio = wav(samples);
  if (stt.path === "relay") {
    const heard = await link.call<VoiceTranscript>("voice.transcribe", { profile: link.profile, session_id: sessionId, audio: base64(audio) });
    return heard.text.trim();
  }
  if (stt.path !== "direct") throw new Error(stt.reason ?? "no speech-to-text provider");
  const form = new FormData();
  form.append("file", new Blob([audio as BlobPart], { type: "audio/wav" }), "speech.wav");
  const headers: Record<string, string> = {};
  let url: string;
  if (stt.wire === "elevenlabs") {
    form.append("model_id", stt.model);
    if (stt.language) form.append("language_code", stt.language);
    headers["xi-api-key"] = stt.api_key ?? "";
    url = `${stt.base_url}/speech-to-text`;
  } else {
    form.append("model", stt.model);
    if (stt.language) form.append("language", stt.language);
    headers.authorization = `Bearer ${stt.api_key ?? ""}`;
    url = `${stt.base_url}/audio/transcriptions`;
  }
  for (const [name, value] of Object.entries(stt.extra ?? {})) form.append(name, String(value));
  const res = await ok(fetch(url, { method: "POST", headers, body: form }));
  return String(((await res.json()) as { text?: unknown }).text ?? "").trim();
}

/** Text as it is spoken: code blocks and Markdown marks left out. */
export function speakable(text: string, stripTags = false): string {
  let out = text
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/`([^`]*)`/g, "$1")
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/[*_#>~]+/g, "")
    .replace(/\s+/g, " ")
    .trim();
  if (stripTags) out = out.replace(/\[[^\]]*\]/g, "").replace(/\s+/g, " ").trim();
  return out;
}

/** 16-bit mono PCM at `rate` as a WAV blob. */
function pcmBlob(pcm: Uint8Array, rate: number): Blob {
  const samples = new Int16Array(pcm.buffer, pcm.byteOffset, Math.floor(pcm.byteLength / 2));
  return new Blob([wav(samples, rate) as BlobPart], { type: "audio/wav" });
}

/** A direct provider's audio reply (bytes, or JSON carrying base64). */
async function audioOf(res: Response): Promise<Blob> {
  const type = res.headers.get("content-type") ?? "";
  if (!type.includes("json")) return res.blob();
  const reply = (await res.json()) as { audio_data?: string; audio?: string; data?: { audio?: string } };
  const b64 = reply.audio_data ?? reply.audio ?? reply.data?.audio;
  if (!b64) throw new Error("the reply holds no audio");
  return new Blob([unbase64(b64) as BlobPart], { type: "audio/mpeg" });
}

type Speaker = (tts: ClientSpeech, text: string) => Promise<Blob>;

/** The direct request of each text-to-speech request shape. */
const SPEAKERS: Record<string, Speaker> = {
  openai: async (tts, text) => {
    const body: Record<string, unknown> = { model: tts.model, input: text, response_format: "mp3", ...(tts.extra ?? {}) };
    if (tts.voice) body.voice = tts.voice;
    const headers = { authorization: `Bearer ${tts.api_key ?? ""}`, "content-type": "application/json" };
    return audioOf(await ok(fetch(`${tts.base_url}/audio/speech`, { method: "POST", headers, body: JSON.stringify(body) })));
  },
  elevenlabs: async (tts, text) => {
    const headers = { "xi-api-key": tts.api_key ?? "", "content-type": "application/json" };
    const url = `${tts.base_url}/text-to-speech/${encodeURIComponent(tts.voice ?? "")}?output_format=mp3_44100_128`;
    const res = await ok(fetch(url, { method: "POST", headers, body: JSON.stringify({ text, model_id: tts.model, ...(tts.extra ?? {}) }) }));
    return res.blob();
  },
  gemini: async (tts, text) => {
    const body = {
      contents: [{ parts: [{ text }] }],
      generationConfig: { responseModalities: ["AUDIO"], speechConfig: { voiceConfig: { prebuiltVoiceConfig: { voiceName: tts.voice } } } },
    };
    const headers = { "x-goog-api-key": tts.api_key ?? "", "content-type": "application/json" };
    const res = await ok(fetch(`${tts.base_url}/models/${tts.model}:generateContent`, { method: "POST", headers, body: JSON.stringify(body) }));
    const reply = (await res.json()) as { candidates?: { content?: { parts?: { inlineData?: { data?: string } }[] } }[] };
    const data = reply.candidates?.[0]?.content?.parts?.[0]?.inlineData?.data;
    if (!data) throw new Error("the reply holds no audio");
    // Gemini speaks 16-bit mono PCM at 24 kHz.
    return pcmBlob(unbase64(data), 24_000);
  },
  minimax: async (tts, text) => {
    const body = { model: tts.model, text, stream: false, voice_setting: { voice_id: tts.voice }, audio_setting: { format: "mp3" } };
    const headers = { authorization: `Bearer ${tts.api_key ?? ""}`, "content-type": "application/json" };
    const res = await ok(fetch(`${tts.base_url}/t2a_v2`, { method: "POST", headers, body: JSON.stringify(body) }));
    const hex = ((await res.json()) as { data?: { audio?: string } }).data?.audio ?? "";
    if (!hex) throw new Error("the reply holds no audio");
    const bytes = new Uint8Array(hex.length / 2);
    for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
    return new Blob([bytes as BlobPart], { type: "audio/mpeg" });
  },
};

/** Speaks `text` with the profile's TTS provider: the audio to play. */
export async function synthesize(link: VoiceLink, tts: ClientSpeech, text: string, sessionId: string | null): Promise<Blob> {
  const said = speakable(text, tts.strip_tags ?? false);
  if (tts.path === "relay") {
    if (!sessionId) throw new Error("no session to speak for");
    const spoken = await link.call<VoiceSpoken>("voice.speak", { profile: link.profile, session_id: sessionId, text: said });
    return new Blob([unbase64(spoken.audio) as BlobPart], { type: spoken.mime });
  }
  const speaker = tts.path === "direct" ? SPEAKERS[tts.wire ?? "openai"] : undefined;
  if (!speaker) throw new Error(tts.reason ?? "no text-to-speech provider");
  return speaker(tts, said);
}

/** Plays audio blobs, one at a time. */
export class Player {
  private audio: HTMLAudioElement | null = null;
  private ended: (() => void) | null = null;

  get playing(): boolean {
    return this.audio !== null;
  }

  /** Plays `blob`; resolves when it ends or is stopped. */
  play(blob: Blob): Promise<void> {
    this.stop();
    const url = URL.createObjectURL(blob);
    const audio = new Audio(url);
    audio.dataset.voice = "playback";
    this.audio = audio;
    return new Promise((resolve) => {
      const done = () => {
        if (this.audio === audio) this.audio = null;
        this.ended = null;
        audio.pause();
        URL.revokeObjectURL(url);
        resolve();
      };
      this.ended = done;
      audio.addEventListener("ended", done);
      audio.addEventListener("error", done);
      audio.play().catch(done);
    });
  }

  /** Stops what plays. */
  stop(): void {
    this.ended?.();
  }
}
