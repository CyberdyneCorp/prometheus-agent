// The desktop's chained voice conversation (spec: voice-mode, Desktop voice
// controls; Barge-in; Spoken and typed stop phrases): listen, transcribe,
// run the turn on the voice route, speak the answer, and listen again
// until nobody speaks or a stop phrase is said. The microphone stays live
// from the end of each utterance until the answer has played: speech
// during generation interrupts the turn and becomes the next message,
// speech during playback stops it and is submitted, both with the
// interruption note.

import type { VoiceClientConfig } from "../../../../schema/rpc";
import { bargeIn, INTERRUPTED_NOTE, isStopPhrase, utterance } from "./listen";
import type { Mic } from "./mic";
import { type Player, synthesize, transcribe, type VoiceLink } from "./speech";

/** What a conversation drives in the window. */
export interface Talker {
  /** Sends a spoken message; resolves with the answer. */
  voiceTurn(text: string): Promise<string | null>;
  /** Interrupts the running turn. */
  interrupt(): Promise<void>;
  /** The chat's session (`null`: still a draft). */
  readonly sessionId: string | null;
}

/** What the conversation is doing, for the window to show. */
export type VoiceState = "listening" | "transcribing" | "thinking" | "speaking" | "idle";

type Outcome = { kind: "reply"; text: string | null } | { kind: "barge" } | { kind: "played" };

export class Conversation {
  private readonly stopped = new AbortController();

  constructor(
    private readonly link: VoiceLink,
    private readonly talker: Talker,
    private readonly player: Player,
    private readonly show: (state: VoiceState) => void,
  ) {}

  /** Ends the conversation (the loop finishes what it awaits and stops). */
  stop(): void {
    this.stopped.abort();
    this.player.stop();
  }

  /** Runs until stopped, a stop phrase, or an utterance nobody spoke. */
  async run(config: VoiceClientConfig, mic: Mic): Promise<void> {
    const listen = config.listen;
    let pending: Int16Array | null = null;
    let note = false;
    try {
      while (!this.stopped.signal.aborted) {
        this.show("listening");
        const samples = pending ?? (await utterance(mic, listen, this.stopped.signal));
        pending = null;
        if (!samples || this.stopped.signal.aborted) return;
        this.show("transcribing");
        const text = await transcribe(this.link, config.stt, samples, this.talker.sessionId);
        if (!text) continue;
        if (isStopPhrase(text, listen.stop_phrases)) return;
        const cut = new AbortController();
        const quit = () => cut.abort();
        this.stopped.signal.addEventListener("abort", quit);
        try {
          const barged = await this.turn(config, mic, note ? `${INTERRUPTED_NOTE}\n${text}` : text, cut.signal);
          note = barged;
          if (barged) pending = await utterance(mic, listen, this.stopped.signal);
        } finally {
          cut.abort();
          this.stopped.signal.removeEventListener("abort", quit);
        }
      }
    } finally {
      this.player.stop();
      this.show("idle");
    }
  }

  /** One turn: true when the user talked over it. */
  private async turn(config: VoiceClientConfig, mic: Mic, text: string, cut: AbortSignal): Promise<boolean> {
    const never = new Promise<boolean>(() => {});
    const barge = (config.listen.barge_in ? bargeIn(mic, config.listen.barge_in_threshold_multiplier, cut) : never).then((b): Outcome => (b ? { kind: "barge" } : { kind: "played" }));
    this.show("thinking");
    const generated = await Promise.race([this.talker.voiceTurn(text).then((t): Outcome => ({ kind: "reply", text: t })), barge]);
    if (generated.kind === "barge") {
      await this.talker.interrupt();
      return true;
    }
    if (generated.kind !== "reply" || !generated.text || cut.aborted) return false;
    const audio = await synthesize(this.link, config.tts, generated.text, this.talker.sessionId);
    if (cut.aborted) return false;
    this.show("speaking");
    const played = await Promise.race([this.player.play(audio).then((): Outcome => ({ kind: "played" })), barge]);
    if (played.kind !== "barge") return false;
    this.player.stop();
    return true;
  }
}
