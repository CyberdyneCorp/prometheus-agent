// The desktop's wake ear (spec: voice-mode, Wake word listener; Wake
// capture location for remote clients; Wake word profile routing): the
// ear arms the profile's wake listener on the daemon with client capture
// and streams the desktop microphone to it as 16 kHz PCM (`wake.feed`); the
// daemon's on-device engine hears the phrase and the desktop then records
// the request on its own microphone. The indicator is solid while armed
// and shows why when arming failed. Wake audio goes only to the daemon.

import type { WakeArmed, WakeDetected } from "../../../../schema/rpc";
import { h } from "../../../../web/src/lib/dom";
import { base64, Mic, RATE } from "./mic";
import type { VoiceLink } from "./speech";

/** PCM sent per `wake.feed` (100 ms). */
const FRAME = RATE / 10;

/** What the window does when the phrase is heard. */
export type OnWake = (detected: WakeDetected, mic: Mic, armed: WakeArmed) => Promise<void>;

export class WakeEar {
  readonly button = h("button", { type: "button", name: "wake-ear", class: "wake-ear", "aria-pressed": "false", "aria-label": "Wake word", title: "Listen for the wake word" }, "Ear");
  readonly hint = h("span", { class: "wake-hint", role: "alert", hidden: true });
  readonly el = h("span", { class: "wake", "data-state": "off" }, this.button, this.hint);
  private mic: Mic | null = null;
  private stopFeed: (() => void) | null = null;
  private armed: WakeArmed | null = null;
  private busy = false;

  constructor(
    private readonly link: VoiceLink,
    private readonly onWake: OnWake,
  ) {
    this.button.addEventListener("click", () => void (this.on ? this.disarm() : this.arm()));
  }

  /** The user wants the ear on (whether or not it is armed right now). */
  get on(): boolean {
    return this.el.dataset.state !== "off" && this.el.dataset.state !== "error";
  }

  /** The phrase was heard (`wake.detected` of this connection). */
  async detected(detected: WakeDetected): Promise<void> {
    const { mic, armed } = this;
    if (!mic || !armed || this.busy) return;
    this.busy = true;
    // The ear stops listening while the request is recorded and answered.
    this.stopFeed?.();
    this.stopFeed = null;
    this.state("busy");
    try {
      await this.onWake(detected, mic, armed);
    } catch (err) {
      console.warn("wake request failed:", err);
    } finally {
      this.busy = false;
    }
    // Listening resumes, on the profile the window shows now.
    if (this.mic === mic) await this.arm(mic);
  }

  /** Arms the listener (again, after a request or a profile switch). */
  async arm(reuse: Mic | null = null): Promise<void> {
    this.state("arming");
    try {
      this.armed = await this.link.call<WakeArmed>("wake.arm", { profile: this.link.profile, client_capture: true });
      this.mic = reuse ?? this.mic ?? (await Mic.open());
      this.feed(this.mic);
      this.state("armed");
    } catch (err) {
      this.release();
      this.failed((err as Error).message);
    }
  }

  /** Turns the ear off. */
  async disarm(): Promise<void> {
    this.release();
    this.state("off");
    await this.link.call("wake.disarm", { profile: this.link.profile }).catch(() => {});
  }

  /** The window's connection changed: the daemon dropped the listener
   *  with the old connection; an ear that was on arms again. */
  async reconnected(): Promise<void> {
    if (this.on && !this.busy) await this.arm(this.mic);
  }

  private feed(mic: Mic): void {
    this.stopFeed?.();
    const profile = this.link.profile;
    let buffered: Int16Array[] = [];
    let length = 0;
    this.stopFeed = mic.listen((pcm) => {
      buffered.push(pcm);
      length += pcm.length;
      if (length < FRAME) return;
      const frame = new Int16Array(length);
      let at = 0;
      for (const b of buffered) {
        frame.set(b, at);
        at += b.length;
      }
      buffered = [];
      length = 0;
      const bytes = new Uint8Array(frame.buffer);
      this.link.call("wake.feed", { profile, pcm: base64(bytes) }).catch((err: unknown) => {
        if (this.el.dataset.state === "armed" && !this.busy) {
          this.release();
          this.failed((err as Error).message);
        }
      });
    });
  }

  private release(): void {
    this.stopFeed?.();
    this.stopFeed = null;
    this.mic?.close();
    this.mic = null;
    this.armed = null;
  }

  private failed(message: string): void {
    this.state("error");
    this.hint.textContent = message;
    this.hint.hidden = false;
    this.button.title = `Wake word not armed: ${message}`;
  }

  private state(state: "off" | "arming" | "armed" | "busy" | "error"): void {
    this.el.dataset.state = state;
    this.button.setAttribute("aria-pressed", String(state === "armed" || state === "busy"));
    if (state !== "error") {
      this.hint.hidden = true;
      this.hint.textContent = "";
      this.button.title = state === "armed" ? "Listening for the wake word" : "Listen for the wake word";
    }
  }
}
