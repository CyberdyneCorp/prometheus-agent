// Tours and tips (spec: desktop-app, Pane focus, tours and tips): the
// agent lists the stable targets of the window (or of the preview page),
// highlights one step,
// runs a paged tour (its own steps or the built-in quick or full tour) or
// shows one arrow bubble. A tour waits for the user to finish or stop it;
// a tip never waits, and a new tip replaces the last one.

import { h } from "../../../web/src/lib/dom";

/** One step: where the bubble points and what it says. */
export interface Step {
  selector: string;
  title?: string;
  text?: string;
  side?: string;
}

type Result = Record<string, unknown>;

/** Shows a step inside the preview page (`null`: the tour ended). */
export type Marker = (step: Step | null) => void;

/** The window's stable targets, by name. */
export const TARGETS: Record<string, string> = {
  sessions: "ul.sessions",
  "new-chat": "button[name=new-chat]",
  composer: "form.composer",
  panes: "nav.pane-tabs",
  terminal: "section.terminals",
  layout: ".layout-controls",
  projects: "section.projects",
  status: ".connection",
};

const PRESETS: Record<string, Step[]> = {
  quick: [
    { selector: TARGETS.composer ?? "", title: "Ask anything", text: "Type here; Enter sends." },
    { selector: TARGETS.sessions ?? "", title: "Your chats", text: "Every conversation of this profile." },
    { selector: TARGETS.panes ?? "", title: "Panes", text: "Files, artifacts, preview and review sit here." },
  ],
  full: [
    { selector: TARGETS.composer ?? "", title: "Ask anything", text: "Type here; Enter sends, / opens commands." },
    { selector: TARGETS.sessions ?? "", title: "Your chats", text: "Every conversation of this profile." },
    { selector: TARGETS["new-chat"] ?? "", title: "New chat", text: "Start fresh at any time." },
    { selector: TARGETS.projects ?? "", title: "Projects", text: "Group chats by repository." },
    { selector: TARGETS.panes ?? "", title: "Panes", text: "Files, artifacts, preview and review." },
    { selector: TARGETS.layout ?? "", title: "Layouts", text: "Simple or advanced, and saved arrangements." },
  ],
};

function placeBubble(bubble: HTMLElement, target: Element, side: string): void {
  const r = target.getBoundingClientRect();
  const at: Record<string, [number, number]> = {
    top: [r.left, r.top - 8],
    bottom: [r.left, r.bottom + 8],
    left: [r.left - 8, r.top],
    right: [r.right + 8, r.top],
  };
  const [x, y] = at[side] ?? at.bottom ?? [0, 0];
  bubble.style.left = `${Math.max(0, x)}px`;
  bubble.style.top = `${Math.max(0, y)}px`;
  bubble.dataset.side = side;
}

export class Guide {
  readonly el = h("div", { class: "guide", "aria-live": "polite" });
  private tip: HTMLElement | null = null;
  private tour: { steps: Step[]; index: number; done: (result: Result) => void; mark?: Marker } | null = null;
  private bubble: HTMLElement | null = null;

  /** `gui_tour`; with `mark`, the steps are in the preview page. */
  run(args: Record<string, unknown>, mark?: Marker): Promise<Result> | Result {
    const action = String(args.action ?? "");
    if (action === "targets") return { success: true, targets: Object.entries(TARGETS).map(([name, selector]) => ({ name, selector, visible: document.querySelector(selector) !== null })) };
    if (action === "show") return this.highlight({ selector: String(args.selector ?? ""), title: args.title as string, text: args.text as string, side: args.side as string });
    if (action === "start") return this.start(args, mark);
    if (!this.tour) return { success: false, error: "no tour is running" };
    if (action === "stop") return this.finish(false);
    const step = this.tour.index + (action === "next" ? 1 : -1);
    return this.go(typeof args.step_index === "number" ? args.step_index : step);
  }

  /** `show_tip`: one bubble, replacing the last; never waits. */
  showTip(step: Step): Result {
    const target = document.querySelector(step.selector);
    if (!target) return { success: false, error: `nothing in the window matches ${step.selector}` };
    this.tip?.remove();
    const close = h("button", { type: "button", name: "dismiss-tip", "aria-label": "Dismiss" }, "×");
    this.tip = h("div", { class: "tip bubble", role: "note" }, step.title ? h("strong", {}, step.title) : "", h("p", {}, step.text ?? ""), close);
    close.addEventListener("click", () => this.tip?.remove());
    this.el.append(this.tip);
    placeBubble(this.tip, target, step.side ?? "bottom");
    return { success: true, shown: true };
  }

  private highlight(step: Step): Result {
    const target = document.querySelector(step.selector);
    if (!target) return { success: false, error: `nothing in the window matches ${step.selector}` };
    this.render(step, target, null);
    return { success: true, shown: step.selector };
  }

  private start(args: Record<string, unknown>, mark?: Marker): Promise<Result> {
    const steps = Array.isArray(args.steps) ? (args.steps as Step[]) : (PRESETS[String(args.preset ?? "quick")] ?? []);
    this.tour?.done({ success: true, completed: false, replaced: true });
    return new Promise((done) => {
      this.tour = { steps, index: 0, done, mark };
      this.go(0);
    });
  }

  private go(index: number): Result {
    const tour = this.tour;
    if (!tour) return { success: false, error: "no tour is running" };
    if (index >= tour.steps.length) return this.finish(true);
    tour.index = Math.max(0, index);
    const step = tour.steps[tour.index] as Step;
    // A preview step is marked in the page; the bubble sits by the rail.
    tour.mark?.(step);
    const target = tour.mark ? document.querySelector("section.preview") : document.querySelector(step.selector);
    this.render(step, target, tour);
    return { success: true, step_index: tour.index, steps: tour.steps.length, found: target !== null };
  }

  private render(step: Step, target: Element | null, tour: { steps: Step[]; index: number } | null): void {
    this.bubble?.remove();
    const controls: HTMLElement[] = [];
    if (tour) {
      const prev = h("button", { type: "button", name: "tour-prev", disabled: tour.index === 0 }, "Back");
      prev.addEventListener("click", () => this.go(tour.index - 1));
      const next = h("button", { type: "button", name: "tour-next" }, tour.index + 1 === tour.steps.length ? "Done" : "Next");
      next.addEventListener("click", () => this.go(tour.index + 1));
      const stop = h("button", { type: "button", name: "tour-stop" }, "Stop");
      stop.addEventListener("click", () => this.finish(false));
      controls.push(h("div", { class: "tour-controls" }, h("span", {}, `${tour.index + 1} / ${tour.steps.length}`), prev, next, stop));
    }
    this.bubble = h("div", { class: "tour bubble", role: "dialog", "data-target": step.selector }, step.title ? h("strong", {}, step.title) : "", h("p", {}, step.text ?? ""), ...controls);
    this.el.append(this.bubble);
    if (target) placeBubble(this.bubble, target, step.side ?? "bottom");
  }

  private finish(completed: boolean): Result {
    const tour = this.tour;
    this.tour = null;
    this.bubble?.remove();
    this.bubble = null;
    const result = { success: true, completed, step_index: tour?.index ?? 0 };
    tour?.mark?.(null);
    tour?.done(result);
    return result;
  }
}
