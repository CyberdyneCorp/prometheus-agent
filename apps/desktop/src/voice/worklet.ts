// The microphone's AudioWorklet (spec: voice-mode, Desktop voice controls;
// Wake capture location for remote clients): it runs on the audio thread
// of a 16 kHz AudioContext and hands the page each 128-sample block as
// 16-bit PCM, the format `wake.feed` and the recorders take. Built into
// apps/desktop/dist/voice-worklet.js and loaded with `addModule`.

declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;

class Pcm16 extends AudioWorkletProcessor {
  process(inputs: Float32Array[][]): boolean {
    const channel = inputs[0]?.[0];
    if (channel && channel.length > 0) {
      const pcm = new Int16Array(channel.length);
      for (let i = 0; i < channel.length; i++) pcm[i] = Math.max(-1, Math.min(1, channel[i] ?? 0)) * 0x7fff;
      this.port.postMessage(pcm, [pcm.buffer]);
    }
    return true;
  }
}

registerProcessor("pcm16", Pcm16);
