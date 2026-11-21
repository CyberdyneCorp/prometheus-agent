// The desktop pet (spec: pets, Desktop pet and pop-out overlay; Affection
// reactions; Achievement unlocks celebrate): the active pet floats in the
// main window, follows the open session's activity, resizes with Alt +
// wheel (`pets.scale`), wanders while the agent is idle when `pets.roam`
// is on, and pops out with Shift + click into the always-on-top overlay
// window (`#pet`). The overlay has no connection: it gets the pet's state
// from this window and hands back what is typed on it, which this window
// submits to the most recent session on its own connection.

import type { Envelope } from "../../../../schema/rpc";
import { h } from "../../../../web/src/lib/dom";
import type { ChatView } from "../chat";
import type { DesktopClient } from "../client";
import { onShellEvent, shell } from "../shell";
import { petsDialog } from "./dialogs";
import { hatchDialog } from "./hatch";
import { Sprite, type SpriteData } from "./sprite";
import { bubble, isAffectionate, type PetState, Tracker } from "./state";

interface PetsState {
  enabled: boolean;
  active: string | null;
  scale: number;
  roam: boolean;
  notice?: string;
}

/** What the overlay sends back. */
type FromOverlay = { type: "ready" } | { type: "submit"; text: string } | { type: "popin" };

const TICK_MS = 250;
const SCALE_STEP = 0.05;

export class FloatingPet {
  readonly el: HTMLElement;
  readonly toasts = h("div", { class: "achievement-toasts", "aria-live": "polite" });
  private readonly sprite = new Sprite("Pet");
  private readonly heart = h("span", { class: "pet-heart", hidden: true }, "♥");
  private readonly tracker = new Tracker();
  private state: PetsState = { enabled: false, active: null, scale: 0.33, roam: false };
  private data: SpriteData | null = null;
  private popped = false;
  private mail = false;
  private shown: string | null = null;
  private saveScale = 0;
  private roamX = 0;
  /** The session the overlay last sent a prompt to. */
  private fromPet: string | null = null;

  constructor(
    private readonly client: DesktopClient,
    private readonly chat: ChatView,
    private readonly say: (text: string, kind?: "info" | "warn" | "error") => void,
    private readonly dialogs: HTMLElement,
  ) {
    this.el = h("div", { class: "floating-pet", hidden: true, title: "Shift+click: pop out · Alt+wheel: resize" }, this.heart, this.sprite.el);
    this.el.addEventListener("click", (e) => {
      if (e.shiftKey) void this.popOut();
    });
    this.el.addEventListener("wheel", (e) => this.wheel(e), { passive: false });
    onShellEvent("pet-message", (payload) => void this.fromOverlay(payload as FromOverlay));
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden && this.mail) {
        this.mail = false;
        this.sync(true);
      }
    });
    window.setInterval(() => this.tick(), TICK_MS);
  }

  /** Reads the profile's pet (on connect and after a change). */
  async load(): Promise<void> {
    try {
      this.state = await this.client.call<PetsState>("pets.state", { profile: this.client.profile });
      const wanted = this.state.enabled ? this.state.active : null;
      if (wanted && wanted !== this.data?.slug) {
        this.data = await this.client.call<SpriteData>("pets.sprite", { profile: this.client.profile, slug: wanted });
        await this.sprite.load(this.data);
      } else if (!wanted) {
        this.data = null;
        await this.sprite.load(null);
      }
      this.sprite.scale = this.state.scale;
      this.el.hidden = !this.data || this.popped;
      this.sync(true);
    } catch (err) {
      console.warn("pet unavailable:", err);
    }
  }

  /** A session event of the window's open session. */
  observe(envelope: Envelope): void {
    if (envelope.event !== "session.event" || envelope.session_id !== this.chat.sessionId) return;
    const data = envelope.data as { type?: string; payload?: Record<string, unknown> };
    if (data.type) this.tracker.event(data.type, data.payload ?? {});
  }

  /** A turn of the open session ended (while hidden: the mail badge). */
  turnFinished(sessionId: string | null, failed: boolean): void {
    if (sessionId === null || (sessionId !== this.chat.sessionId && sessionId !== this.fromPet)) return;
    this.tracker.turnEnded(failed);
    if (document.hidden) this.mail = true;
    this.sync(true);
  }

  /** The user sent `text` (unchanged); affection plays the heart. */
  sent(text: string): void {
    if (isAffectionate(text)) this.tracker.heart();
  }

  /** `achievement.unlocked`: a dismissible toast, and the pet jumps. */
  unlocked(data: { id: string; tier: string; name?: string }): void {
    this.tracker.celebrate();
    const close = h("button", { type: "button", name: "dismiss", "aria-label": "Dismiss" }, "×");
    const toast = h("div", { class: "achievement-toast", role: "status", "data-id": data.id }, h("strong", {}, "Achievement unlocked"), h("span", {}, `${title(data.id)} · ${data.tier}`), close);
    close.addEventListener("click", () => toast.remove());
    this.toasts.append(toast);
    this.sync(true);
  }

  /** `/pet …` and `/hatch …` typed in the desktop's composer. */
  async command(name: string, args: string, sessionId: string | null = null): Promise<void> {
    if (name === "hatch" || name === "generate-pet") {
      return hatchDialog(this.client, this.dialogs, args, () => void this.load(), sessionId);
    }
    const [word = "", value = ""] = args.trim().split(/\s+/);
    if (word === "list") return petsDialog(this.client, this.dialogs, () => void this.load(), (n, a) => void this.command(n, a));
    const params: Record<string, unknown> = { profile: this.client.profile };
    if (word === "") params.enabled = !this.state.enabled;
    else if (word === "off") params.enabled = false;
    else if (word === "scale") params.scale = Number(value);
    else params.slug = word;
    try {
      const done = await this.client.call<PetsState>("pets.set", params);
      if (done.notice) this.say(done.notice);
      await this.load();
    } catch (err) {
      this.say((err as Error).message, "error");
    }
  }

  private wheel(e: WheelEvent): void {
    if (!e.altKey || !this.data) return;
    e.preventDefault();
    const scale = Math.min(3, Math.max(0.1, Math.round((this.sprite.scale - Math.sign(e.deltaY) * SCALE_STEP) * 100) / 100));
    this.sprite.scale = scale;
    this.state.scale = scale;
    window.clearTimeout(this.saveScale);
    this.saveScale = window.setTimeout(() => {
      void this.client.call("pets.set", { profile: this.client.profile, scale }).catch((err: unknown) => console.warn("pets.scale:", err));
    }, 300);
    this.sync(true);
  }

  private async popOut(): Promise<void> {
    try {
      await shell("pet_overlay", { open: true });
      this.popped = true;
      this.el.hidden = true;
    } catch (err) {
      this.say(`The pet cannot pop out here: ${String(err)}`, "warn");
    }
  }

  private async popIn(): Promise<void> {
    await shell("pet_overlay", { open: false }).catch(() => {});
    this.popped = false;
    this.el.hidden = !this.data;
  }

  private async fromOverlay(message: FromOverlay): Promise<void> {
    if (message.type === "ready") {
      await this.relay({ type: "sprite", data: this.data, scale: this.sprite.scale });
      this.sync(true);
    } else if (message.type === "popin") {
      await this.popIn();
    } else if (message.type === "submit") {
      await this.submitToRecent(message.text);
    }
  }

  /** A prompt from the overlay: the most recent session, on this
   *  window's connection, without bringing the app forward. */
  private async submitToRecent(text: string): Promise<void> {
    const sessionId = this.chat.sessionId ?? (await this.recentSession()) ?? (await this.chat.session());
    this.fromPet = sessionId;
    this.sent(text);
    try {
      await this.client.call("prompt.submit", { session_id: sessionId, text, files: [] });
    } catch (err) {
      this.say(`The pet could not send that: ${(err as Error).message}`, "error");
    }
  }

  private async recentSession(): Promise<string | null> {
    const listed = await this.client.call<{ sessions: { session_id: string }[] }>("session.list", { profile: this.client.profile }).catch(() => ({ sessions: [] }));
    return listed.sessions[0]?.session_id ?? null;
  }

  private relay(message: unknown): Promise<void> {
    return shell<unknown>("pet_relay", { to: "pet", message }).then(
      () => undefined,
      () => undefined,
    );
  }

  private tick(): void {
    const state: PetState = this.tracker.state();
    this.sprite.state = state;
    this.heart.hidden = !this.tracker.hearts();
    this.el.dataset.state = state;
    if (this.state.roam && state === "idle" && !this.el.hidden) this.roam();
    this.sync(false);
  }

  /** Wanders a little along the window's bottom edge. */
  private roam(): void {
    const room = Math.max(0, window.innerWidth - this.el.offsetWidth - 32);
    if (Math.random() < 0.1) this.roamX = Math.round(Math.random() * room);
    this.el.style.right = `${16 + this.roamX}px`;
    this.el.dataset.roaming = "true";
  }

  /** Tells the overlay what it shows (when it changed, or `force`). */
  private sync(force: boolean): void {
    if (!this.popped) return;
    const state = this.tracker.state();
    const shown = JSON.stringify([state, this.mail, this.tracker.hearts(), this.sprite.scale]);
    if (!force && shown === this.shown) return;
    this.shown = shown;
    void this.relay({ type: "state", state, bubble: bubble(state), mail: this.mail, heart: this.tracker.hearts(), scale: this.sprite.scale });
  }
}

/** An achievement id as a name (`night-owl` → `Night Owl`). */
function title(id: string): string {
  return id
    .split("-")
    .map((w) => (w ? w[0]?.toUpperCase() + w.slice(1) : w))
    .join(" ");
}
