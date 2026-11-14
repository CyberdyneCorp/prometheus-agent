// The first-run onboarding chat (spec: desktop-app, First-run onboarding
// chat): on the first launch the window opens the profile's onboarding
// chat (`onboarding.ensure`, the only session with `setup_beat`) and the
// agent walks the user through setup cards: name, theme, layout,
// connectors, plugins, tour and build. Each card waits for the user (the
// daemon gives up after 300 s and the card closes); a suggested plugin is
// installed only through the catalog's consent dialog; only Skip setup
// ends onboarding early.

import { h } from "../../../web/src/lib/dom";
import type { Prefs } from "../../../web/src/lib/prefs";
import type { ChatView } from "./chat";
import type { DesktopClient } from "./client";
import { installDialog } from "./install";

type Result = Record<string, unknown>;

/** The machine preference that records onboarding (`started`, `done`,
 *  `skipped`). */
const PREF = "onboarding";
const KICKOFF = "Hi! Please help me set up Prometheus.";

const DEFAULT_OPTIONS: Record<string, string[]> = {
  theme: ["system", "light", "dark"],
  layout: ["default", "basic", "focus", "sidebar-left"],
  tour: ["Show me around", "Not now"],
};

const TITLES: Record<string, string> = {
  name: "What should I call you?",
  theme: "Pick a look",
  layout: "Choose a layout",
  connectors: "Connect your tools",
  plugins: "Suggested plugins",
  tour: "A quick tour?",
  build: "What shall we build first?",
};

export class Onboarding {
  readonly skip = h("button", { type: "button", name: "skip-setup", hidden: true }, "Skip setup");
  private pending: ((result: Result) => void) | null = null;

  constructor(
    private readonly client: DesktopClient,
    private readonly chat: ChatView,
    private readonly prefs: Prefs,
    private readonly dialogs: HTMLElement,
  ) {
    this.skip.addEventListener("click", () => void this.skipSetup());
  }

  /** Opens the onboarding chat when this machine has not been set up. */
  async firstRun(): Promise<void> {
    if (this.prefs.values[PREF] !== undefined) return;
    const ref = await this.client.call<{ session_id: string; created: boolean }>("onboarding.ensure", { profile: this.client.profile });
    await this.prefs.set(PREF, "started");
    await this.chat.open(ref.session_id);
    this.skip.hidden = false;
    if (ref.created) await this.client.call("prompt.submit", { session_id: ref.session_id, text: KICKOFF });
  }

  /** `setup_beat`: shows the card and waits for the user's answer. */
  beat(requestId: string, sessionId: string, args: Record<string, unknown>): Promise<Result> {
    const beat = String(args.beat ?? "");
    this.skip.hidden = false;
    return new Promise((resolve) => {
      const answer = (result: Result) => {
        this.pending = null;
        card.querySelectorAll("button, input").forEach((c) => ((c as HTMLInputElement).disabled = true));
        if (beat === "build") {
          this.skip.hidden = true;
          void this.prefs.set(PREF, "done");
        }
        resolve(result);
      };
      this.pending = answer;
      const card = h("section", { class: "setup-card card", "data-beat": beat, role: "group", "aria-label": TITLES[beat] ?? beat }, h("h3", {}, TITLES[beat] ?? beat), args.question ? h("p", {}, String(args.question)) : "", this.controls(beat, args, answer), h("p", { class: "card-status" }));
      if (this.chat.sessionId === sessionId) this.chat.transcript.card(requestId, card);
      else this.dialogs.append(card);
    });
  }

  private controls(beat: string, args: Record<string, unknown>, answer: (r: Result) => void): HTMLElement {
    if (beat === "plugins") return this.plugins(args, answer);
    const options = Array.isArray(args.options) ? args.options.map(String) : (DEFAULT_OPTIONS[beat] ?? []);
    if (options.length > 0) {
      return h(
        "div",
        { class: "setup-options" },
        ...options.map((option) => {
          const pick = h("button", { type: "button", name: "setup-option", "data-option": option }, option);
          pick.addEventListener("click", () => answer({ answer: option }));
          return pick;
        }),
      );
    }
    const input = h("input", { name: "setup-answer", "aria-label": TITLES[beat] ?? beat, value: typeof args.value === "string" ? args.value : "" });
    const send = h("button", { type: "button", name: "setup-send" }, "Continue");
    send.addEventListener("click", () => answer({ answer: input.value }));
    return h("div", { class: "setup-input" }, input, send);
  }

  /** At most five suggestions; each installs through the consent dialog. */
  private plugins(args: Record<string, unknown>, answer: (r: Result) => void): HTMLElement {
    const suggested = (Array.isArray(args.suggest) ? args.suggest.map(String) : []).slice(0, 5);
    const opened: string[] = [];
    const rows = suggested.map((id) => {
      const install = h("button", { type: "button", name: "setup-install", "data-plugin": id }, `Install ${id}…`);
      install.addEventListener("click", () => {
        opened.push(id);
        void installDialog({ open: "install_plugin", id }, this.client.profile, this.dialogs);
      });
      return h("li", {}, install);
    });
    const done = h("button", { type: "button", name: "setup-send" }, "Continue");
    done.addEventListener("click", () => answer({ answer: { reviewed: opened } }));
    return h("div", { class: "setup-plugins" }, h("ul", {}, ...rows), done);
  }

  /** Skip setup: the only way to end onboarding early. */
  private async skipSetup(): Promise<void> {
    this.skip.hidden = true;
    await this.prefs.set(PREF, "skipped");
    this.pending?.({ answer: null, ended: true });
  }
}
