// The desktop's voice controls (spec: voice-mode, Desktop voice controls;
// Client-direct voice for remote daemons; Realtime full-duplex voice chat
// mode (upgrade); Wake word profile routing): the composer microphone
// dictates one utterance into the input box, each assistant message can
// be read aloud, the conversation button runs the chained voice loop (or
// the realtime session), and the wake ear sits in the window's header.
// Dictation, the conversation and the ear need a speech-to-text provider,
// read aloud a text-to-speech one: each control is hidden while its
// provider does not resolve for the window's profile.

import type { VoiceClientConfig, WakeArmed, WakeDetected } from "../../../../schema/rpc";
import { h } from "../../../../web/src/lib/dom";
import { Conversation, type Talker, type VoiceState } from "./conversation";
import { WakeEar } from "./ear";
import { utterance } from "./listen";
import { Mic } from "./mic";
import { RealtimeVoice } from "./realtime";
import { Player, synthesize, transcribe, type VoiceLink, voiceConfig } from "./speech";

/** What the controls need of the window. */
export interface VoiceWindow {
  readonly link: VoiceLink;
  readonly chat: Talker & {
    session(): Promise<string>;
    draft(): void;
    readonly composer: { readonly input: HTMLTextAreaElement };
    readonly transcript: { onAssistant: ((el: HTMLElement) => void) | null; readonly el: HTMLElement };
    readonly el: HTMLElement;
  };
  /** Shows another profile (a wake phrase of that profile). */
  switchProfile(profile: string): Promise<void>;
  /** Tells the user something went wrong. */
  say(text: string, kind: "error" | "warn" | "info"): void;
}

const LABELS: Record<VoiceState, string> = {
  listening: "Listening…",
  transcribing: "Transcribing…",
  thinking: "Thinking…",
  speaking: "Speaking…",
  idle: "",
};

export class VoiceControls {
  readonly dictate = h("button", { type: "button", name: "dictate", "aria-label": "Dictate", title: "Dictate into the message box", hidden: true }, "Mic");
  readonly talk = h("button", { type: "button", name: "voice-chat", "aria-pressed": "false", "aria-label": "Voice conversation", hidden: true }, "Talk");
  readonly pill = h("span", { class: "voice-pill", role: "status", hidden: true });
  readonly ear: WakeEar;
  private readonly player = new Player();
  private conversation: Conversation | RealtimeVoice | null = null;
  /** The message being read aloud. */
  private reading: HTMLButtonElement | null = null;
  private dictating: AbortController | null = null;

  constructor(private readonly w: VoiceWindow) {
    this.ear = new WakeEar(w.link, (detected, mic, armed) => this.woken(detected, mic, armed));
    this.ear.el.hidden = true;
    this.dictate.addEventListener("click", () => void this.dictation());
    this.talk.addEventListener("click", () => void this.toggleConversation());
    w.chat.transcript.onAssistant = (el) => this.readAloudButton(el);
    // Typing stops a spoken reply (spec: voice-mode, Barge-in).
    w.chat.composer.input.addEventListener("input", () => this.player.stop());
  }

  /** The controls the composer shows. */
  get composerControls(): HTMLElement[] {
    return [this.dictate, this.talk, this.pill];
  }

  /** Re-reads which providers resolve for the window's profile (on every
   *  connect and profile switch); no credential is kept. */
  async refresh(): Promise<void> {
    let config: VoiceClientConfig | null = null;
    try {
      config = await voiceConfig(this.w.link, false);
    } catch (err) {
      console.warn("voice settings unavailable:", err);
    }
    const hearing = config !== null && config.stt.path !== "none";
    const speaking = config !== null && config.tts.path !== "none";
    this.dictate.hidden = !hearing;
    this.talk.hidden = !(hearing && speaking);
    this.ear.el.hidden = !(hearing && speaking);
    this.w.chat.el.dataset.readAloud = String(speaking);
    await this.ear.reconnected();
  }

  /** One-shot dictation into the composer. */
  private async dictation(): Promise<void> {
    if (this.dictating) return this.dictating.abort();
    const stop = new AbortController();
    this.dictating = stop;
    this.dictate.setAttribute("aria-pressed", "true");
    let mic: Mic | null = null;
    try {
      const config = await voiceConfig(this.w.link, true);
      mic = await Mic.open();
      this.show("listening");
      const samples = await utterance(mic, config.listen, stop.signal);
      mic.close();
      mic = null;
      if (!samples) return;
      this.show("transcribing");
      const text = await transcribe(this.w.link, config.stt, samples, this.w.chat.sessionId);
      const input = this.w.chat.composer.input;
      input.value = input.value ? `${input.value} ${text}` : text;
      input.dispatchEvent(new Event("input"));
      input.focus();
    } catch (err) {
      this.w.say(`Dictation failed: ${(err as Error).message}`, "error");
    } finally {
      mic?.close();
      this.dictating = null;
      this.dictate.setAttribute("aria-pressed", "false");
      this.show("idle");
    }
  }

  private endConversation(): void {
    this.conversation?.stop();
    this.conversation = null;
    this.talk.setAttribute("aria-pressed", "false");
  }

  private async toggleConversation(): Promise<void> {
    if (this.conversation) return this.endConversation();
    this.talk.setAttribute("aria-pressed", "true");
    try {
      // The realtime session runs on the daemon's key: no credential is
      // asked for it.
      const mode = await voiceConfig(this.w.link, false);
      if (mode.chat_mode === "realtime") await this.realtime();
      else await this.chained(await voiceConfig(this.w.link, true));
    } catch (err) {
      this.w.say(`Voice conversation failed: ${(err as Error).message}`, "error");
      this.endConversation();
    }
  }

  private async realtime(): Promise<void> {
    const voice = new RealtimeVoice(this.w.link, this.w.chat, (s) => this.show(s));
    this.conversation = voice;
    await voice.start();
  }

  private async chained(config: VoiceClientConfig, mic?: Mic): Promise<void> {
    const conversation = new Conversation(this.w.link, this.w.chat, this.player, (s) => this.show(s));
    this.conversation = conversation;
    const own = mic ?? (await Mic.open());
    try {
      await conversation.run(config, own);
    } finally {
      if (!mic) own.close();
      if (this.conversation === conversation) {
        this.conversation = null;
        this.talk.setAttribute("aria-pressed", "false");
      }
    }
  }

  /** The wake phrase: another profile's switches the window to it; the
   *  request is recorded on the ear's microphone and answered aloud. */
  private async woken(detected: WakeDetected, mic: Mic, armed: WakeArmed): Promise<void> {
    const other = detected.profile !== this.w.link.profile;
    if (other) await this.w.switchProfile(detected.profile);
    if (other || armed.start_new_session) this.w.chat.draft();
    const config = await voiceConfig(this.w.link, true);
    const samples = await utterance(mic, config.listen);
    if (!samples) return;
    this.show("transcribing");
    const text = await transcribe(this.w.link, config.stt, samples, this.w.chat.sessionId);
    if (!text) return this.show("idle");
    this.show("thinking");
    const reply = await this.w.chat.voiceTurn(text);
    if (reply) {
      const audio = await synthesize(this.w.link, config.tts, reply, this.w.chat.sessionId);
      this.show("speaking");
      await this.player.play(audio);
    }
    this.show("idle");
  }

  /** A read-aloud button on an assistant message (hidden by CSS while no
   *  text-to-speech provider resolves). */
  private readAloudButton(el: HTMLElement): void {
    const button = h("button", { type: "button", name: "read-aloud", class: "read-aloud", "aria-pressed": "false", "aria-label": "Read aloud" }, "Read aloud");
    button.addEventListener("click", () => void this.readAloud(button, el));
    el.append(button);
  }

  private async readAloud(button: HTMLButtonElement, el: HTMLElement): Promise<void> {
    const again = this.reading === button;
    this.reading = null;
    this.player.stop();
    if (again) return;
    const text = el.querySelector(".text")?.textContent ?? "";
    this.reading = button;
    button.setAttribute("aria-pressed", "true");
    try {
      const config = await voiceConfig(this.w.link, true);
      const sessionId = this.w.chat.sessionId ?? (await this.w.chat.session());
      const audio = await synthesize(this.w.link, config.tts, text, sessionId);
      if (this.reading === button) await this.player.play(audio);
    } catch (err) {
      this.w.say(`Read aloud failed: ${(err as Error).message}`, "error");
    } finally {
      if (this.reading === button) this.reading = null;
      button.setAttribute("aria-pressed", "false");
    }
  }

  private show(state: VoiceState): void {
    this.w.chat.el.dataset.voice = state;
    this.pill.textContent = LABELS[state];
    this.pill.hidden = state === "idle";
  }
}
