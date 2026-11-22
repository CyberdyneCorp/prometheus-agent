// The desktop's microphone (spec: voice-mode, Desktop voice controls): one
// 16 kHz mono capture through the `pcm16` worklet, its level for the
// recorders and barge-in, and its frames for the wake ear. Nothing here
// leaves the page; the callers decide where audio goes.

/** The capture rate of every recording and wake frame. */
export const RATE = 16_000;

/** RMS of 16-bit samples, on the same 0-32767 scale as
 *  `voice.silence_threshold`. */
export function rms(pcm: Int16Array): number {
  if (pcm.length === 0) return 0;
  let sum = 0;
  for (const s of pcm) sum += s * s;
  return Math.sqrt(sum / pcm.length);
}

/** An open microphone. */
export class Mic {
  private readonly listeners = new Set<(pcm: Int16Array) => void>();

  private constructor(
    private readonly stream: MediaStream,
    private readonly context: AudioContext,
  ) {}

  /** Opens the default input device (the OS asks for permission once). */
  static async open(): Promise<Mic> {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: false } });
    const context = new AudioContext({ sampleRate: RATE });
    try {
      await context.audioWorklet.addModule(new URL("voice-worklet.js", document.baseURI).href);
      const node = new AudioWorkletNode(context, "pcm16");
      const mic = new Mic(stream, context);
      node.port.onmessage = (event: MessageEvent<Int16Array>) => mic.emit(event.data);
      context.createMediaStreamSource(stream).connect(node);
      return mic;
    } catch (err) {
      for (const track of stream.getTracks()) track.stop();
      void context.close();
      throw err;
    }
  }

  /** Calls `listener` with every block until the returned function runs. */
  listen(listener: (pcm: Int16Array) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** Releases the device. */
  close(): void {
    this.listeners.clear();
    for (const track of this.stream.getTracks()) track.stop();
    void this.context.close();
  }

  private emit(pcm: Int16Array): void {
    for (const listener of this.listeners) listener(pcm);
  }
}

/** A WAV file of 16 kHz mono 16-bit `samples`. */
export function wav(samples: Int16Array, rate = RATE): Uint8Array {
  const out = new Uint8Array(44 + samples.length * 2);
  const view = new DataView(out.buffer);
  const text = (at: number, s: string) => [...s].forEach((c, i) => view.setUint8(at + i, c.charCodeAt(0)));
  text(0, "RIFF");
  view.setUint32(4, 36 + samples.length * 2, true);
  text(8, "WAVE");
  text(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, rate, true);
  view.setUint32(28, rate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  text(36, "data");
  view.setUint32(40, samples.length * 2, true);
  samples.forEach((s, i) => view.setInt16(44 + i * 2, s, true));
  return out;
}

/** Base64 of `bytes` (RPC payloads). */
export function base64(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}

/** The bytes of base64 `text`. */
export function unbase64(text: string): Uint8Array {
  return Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
}
