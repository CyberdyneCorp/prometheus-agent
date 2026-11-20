// Listening as the CLI and TUI do (spec: voice-mode, Two-stage silence
// detection; Barge-in), on the desktop's microphone: an utterance is
// confirmed once the level stays above `silence_threshold` for
// `speech_confirm_ms` (dips under 200 ms tolerated) and ends after
// `silence_duration_ms` of quiet; while a reply generates or plays, the
// barge-in monitor measures the quiet room for the turn's first 200 ms and
// fires once speech stays `barge_in_threshold_multiplier` times above it
// for 160 ms.

import type { VoiceListening } from "../../../../schema/rpc";
import { type Mic, RATE, rms } from "./mic";

/** Dips shorter than this do not reset speech confirmation. */
const DIP_MS = 200;
/** The room is measured for this long at turn start. */
export const CALIBRATION_MS = 200;
/** The lowest floor: digital silence would make any click a barge-in. */
export const MIN_FLOOR = 30;
/** Speech over the trigger must last this long. */
export const TRIGGER_MS = 160;
/** The note the next spoken message carries after a barge-in. */
export const INTERRUPTED_NOTE = "[Your previous spoken reply was cut off: the user started talking over it.]";

const ms = (pcm: Int16Array) => (pcm.length * 1000) / RATE;

/** Records one utterance: its samples, or `null` when nobody spoke (or
 *  `signal` aborted first). */
export function utterance(mic: Mic, listen: VoiceListening, signal?: AbortSignal): Promise<Int16Array | null> {
  return new Promise((resolve) => {
    const chunks: Int16Array[] = [];
    let elapsed = 0;
    let loud = 0;
    let dip = 0;
    let quiet = 0;
    let confirmed = false;
    let over = false;
    const finish = (spoke: boolean) => {
      if (over) return;
      over = true;
      stop();
      signal?.removeEventListener("abort", aborted);
      resolve(spoke ? concat(chunks) : null);
    };
    const aborted = () => finish(false);
    signal?.addEventListener("abort", aborted);
    const stop = mic.listen((pcm) => {
      const step = ms(pcm);
      chunks.push(pcm);
      elapsed += step;
      const speaking = rms(pcm) > listen.silence_threshold;
      if (!confirmed) {
        dip = speaking ? 0 : dip + step;
        if (speaking) loud += step;
        else if (dip >= DIP_MS) loud = 0;
        confirmed = loud >= listen.speech_confirm_ms;
        if (!confirmed && elapsed >= listen.no_speech_timeout_secs * 1000) finish(false);
        return;
      }
      quiet = speaking ? 0 : quiet + step;
      if (quiet >= listen.silence_duration_ms || elapsed >= listen.max_recording_secs * 1000) finish(true);
    });
    if (signal?.aborted) finish(false);
  });
}

function concat(chunks: Int16Array[]): Int16Array {
  const out = new Int16Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

/** Barge-in for one turn: resolves when the user talks over it. */
export function bargeIn(mic: Mic, multiplier: number, signal: AbortSignal): Promise<boolean> {
  return new Promise((resolve) => {
    let calibrated = 0;
    let sum = 0;
    let blocks = 0;
    let trigger: number | null = null;
    let run = 0;
    let over = false;
    const done = (barged: boolean) => {
      if (over) return;
      over = true;
      stop();
      signal.removeEventListener("abort", aborted);
      resolve(barged);
    };
    const aborted = () => done(false);
    signal.addEventListener("abort", aborted);
    const stop = mic.listen((pcm) => {
      const level = rms(pcm);
      const step = ms(pcm);
      if (trigger === null) {
        sum += level;
        blocks += 1;
        calibrated += step;
        if (calibrated >= CALIBRATION_MS) trigger = Math.max(sum / blocks, MIN_FLOOR) * multiplier;
        return;
      }
      run = level > trigger ? run + step : 0;
      if (run >= TRIGGER_MS) done(true);
    });
    if (signal.aborted) done(false);
  });
}

/** `text` is one of the stop phrases (case and surrounding punctuation
 *  ignored). */
export function isStopPhrase(text: string, phrases: string[]): boolean {
  const norm = (s: string) => s.toLowerCase().replace(/^[\s\p{P}]+|[\s\p{P}]+$/gu, "");
  const said = norm(text);
  return phrases.some((p) => norm(p) === said);
}
