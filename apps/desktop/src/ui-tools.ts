// The desktop UI toolset in the window (spec: desktop-app, Desktop UI
// toolset contract and the requirements after it): the daemon sends a
// window tool as `ui.<tool>` to the windows that advertised `desktop_ui`
// (it checked the arguments already); this table runs it and answers.
// Effects that move focus apply only in the focused window: a window in
// the background lets a focused one answer first and then reports that it
// moved nothing, so a background turn never steals focus.

import { applyMode, type Prefs } from "../../../web/src/lib/prefs";
import { h } from "../../../web/src/lib/dom";
import type { ServerRequest } from "../../../web/src/lib/rpc";
import type { ChatView } from "./chat";
import type { DesktopClient } from "./client";
import type { Layout } from "./layout";
import type { Onboarding } from "./onboarding";
import type { PreviewRail } from "./preview";
import type { PreviewDriver } from "./preview-driver";
import { shell } from "./shell";
import type { TerminalPane } from "./terminal";
import type { Guide, Step } from "./tour";

type Args = Record<string, unknown>;
type Result = Record<string, unknown>;

/** What the window's tools act on. */
export interface UiParts {
  chat: ChatView;
  terminal: TerminalPane;
  preview: PreviewRail;
  driver: PreviewDriver;
  layout: Layout;
  prefs: Prefs;
  guide: Guide;
  onboarding: Onboarding;
  dialogs: HTMLElement;
}

/** The colour of each named accent (`theme`: the theme's own). */
export const ACCENTS: Record<string, string | null> = {
  mono: "#868e96",
  green: "#2f9e44",
  cyan: "#1098ad",
  violet: "#7048e8",
  pink: "#d6336c",
  red: "#e03131",
  orange: "#e8590c",
  theme: null,
};

/** Tools whose effect moves the user's focus. */
const MOVES_FOCUS = new Set(["focus_pane", "gui_tour", "show_tip"]);
/** How long a background window lets a focused one answer first. */
const BACKGROUND_GRACE_MS = 750;

const sleep = (ms: number) => new Promise((resolve) => window.setTimeout(resolve, ms));

/** The user's switches for the agent's window tools, machine-wide;
 *  a change applies from the next session on. */
export function toolPrefsControls(prefs: Prefs): HTMLElement {
  const toggles: [string, string, boolean][] = [
    ["message_reactions", "Let the agent react to messages", false],
    ["tours_enabled", "Guided tours", true],
    ["tips_enabled", "In-app tips", true],
  ];
  const boxes = toggles.map(([key, label, fallback]) => {
    const box = h("input", { type: "checkbox", name: key });
    box.addEventListener("change", () => void prefs.set(key, box.checked).catch((err: unknown) => console.warn(`${key}:`, err)));
    prefs.onChange((values) => {
      box.checked = typeof values[key] === "boolean" ? (values[key] as boolean) : fallback;
    });
    return h("label", {}, box, ` ${label}`);
  });
  return h("fieldset", { class: "tool-prefs" }, h("legend", {}, "Agent in the window (from the next chat)"), ...boxes);
}

export class UiTools {
  private readonly handlers: Record<string, (args: Args, request: ServerRequest) => Result | Promise<Result>>;

  constructor(
    private readonly client: DesktopClient,
    private readonly parts: UiParts,
  ) {
    const p = parts;
    this.handlers = {
      read_terminal: (args) => p.terminal.read(args),
      close_terminal: (args) => p.terminal.closeMirror(String(args.process_id ?? "")),
      desktop_preview: (args, request) => this.preview(args, String(request.params.session_id ?? "")),
      drive_preview: (args) => p.driver.drive(args),
      annotate_preview: (args) => p.driver.annotate(args),
      read_window_below: () => shell<Result>("read_window_below"),
      focus_pane: (args) => this.focusPane(String(args.pane ?? "")),
      gui_tour: (args) => (args.surface === "preview" ? this.previewTour(args) : p.guide.run(args)),
      show_tip: (args) => p.guide.showTip(args as unknown as Step),
      set_theme: (args) => this.setTheme(args),
      setup_beat: (args, request) => p.onboarding.beat(request.id, String(request.params.session_id ?? ""), args),
    };
  }

  /** Runs a `ui.*` request; `false` for any other request. */
  handle(request: ServerRequest): boolean {
    if (!request.method.startsWith("ui.")) return false;
    const tool = request.method.slice(3);
    const args = (request.params.arguments ?? {}) as Args;
    void this.run(tool, args, request)
      .catch((err: unknown) => ({ success: false, error: String(err) }))
      .then((result) => this.client.answer(request.id, result));
    return true;
  }

  private async run(tool: string, args: Args, request: ServerRequest): Promise<Result> {
    const handler = this.handlers[tool];
    if (!handler) return { success: false, error: `this window has no ${tool}` };
    if (MOVES_FOCUS.has(tool) && !document.hasFocus()) {
      await sleep(BACKGROUND_GRACE_MS);
      return { success: true, applied: false, note: "the window is in the background; nothing moved" };
    }
    return handler(args, request);
  }

  private async preview(args: Args, sessionId: string): Promise<Result> {
    const { preview, layout, driver } = this.parts;
    const action = String(args.action ?? "");
    if (action === "read") return driver.read(args);
    if (action === "close") {
      const label = typeof args.label === "string" ? args.label : null;
      if (label && preview.page?.label !== label) return { success: false, error: `no preview tab is labelled ${label}` };
      preview.close();
      if (!label) preview.hide();
      return { success: true, closed: label ?? "the preview rail" };
    }
    const url = String(args.url ?? "");
    const web = /^https?:\/\//i.test(url);
    layout.change({ pane: "preview", right: true });
    const label = typeof args.label === "string" ? args.label : undefined;
    const opened = await preview.open(web ? { url } : { sessionId, path: url.replace(/^file:\/\//, "") }, label);
    if (!opened) return { success: false, error: preview.problem || `the preview could not open ${url}` };
    return { success: true, opened: url, kind: web ? "browser" : "file", label: preview.page?.label };
  }

  /** A tour of the preview page: its elements are the targets and each
   *  step is a labelled mark in the page. */
  private async previewTour(args: Args): Promise<Result> {
    const { driver, guide } = this.parts;
    const action = String(args.action ?? "");
    if (action === "targets") {
      const listed = await driver.drive({ action: "elements" });
      return { ...listed, surface: "preview", targets: listed.elements };
    }
    const mark = (step: Step | null) => {
      void driver.annotate({ action: "clear" }).then(() => step && driver.annotate({ action: "add", selector: step.selector, label: step.title ?? step.text ?? "" }));
    };
    if (action === "show") {
      await driver.annotate({ action: "clear" });
      return driver.annotate({ action: "add", selector: args.selector, label: String(args.title ?? args.text ?? "") });
    }
    return guide.run(args, mark);
  }

  private focusPane(pane: string): Result {
    const { layout, chat } = this.parts;
    const reveal: Record<string, () => void> = {
      chat: () => chat.focusComposer(),
      files: () => layout.change({ pane: "files", right: true }),
      terminal: () => layout.change({ terminal: true }),
      review: () => layout.change({ pane: "review", right: true }),
      sessions: () => layout.change({ left: true }),
    };
    const run = reveal[pane];
    if (!run) return { success: false, error: `unknown pane ${pane}` };
    run();
    return { success: true, pane, applied: true };
  }

  /** `set_theme`: everything is checked before anything changes. */
  private async setTheme(args: Args): Promise<Result> {
    const { layout, prefs } = this.parts;
    if (typeof args.picker === "string") return this.picker(args.picker);
    const name = typeof args.layout === "string" ? args.layout : null;
    if (name !== null) {
      const known = layout.names();
      if (![...known.presets, ...known.saved].includes(name)) {
        return { success: false, error: `unknown layout '${name}'; presets: ${known.presets.join(", ")}; saved layouts: ${known.saved.join(", ") || "none"}` };
      }
    }
    const applied: Result = {};
    if (typeof args.accent === "string") {
      const colour = args.accent in ACCENTS ? (ACCENTS[args.accent] ?? null) : args.accent;
      await prefs.set("accent", colour);
      if (colour) document.documentElement.style.setProperty("--accent", colour);
      else document.documentElement.style.removeProperty("--accent");
      applied.accent = args.accent;
    }
    if (typeof args.mode === "string") {
      await prefs.set("mode", args.mode);
      applyMode(args.mode);
      applied.mode = args.mode;
    }
    if (name !== null && layout.applyNamed(name)) applied.layout = name;
    return { success: true, applied };
  }

  /** A picker card; resolves with the user's choice (the daemon stops
   *  waiting after 300 s). */
  private picker(kind: string): Promise<Result> {
    const choices = kind === "accent" ? Object.keys(ACCENTS) : [...this.parts.layout.names().presets, ...this.parts.layout.names().saved];
    return new Promise((resolve) => {
      const card = h("section", { class: "ui-picker card", "data-picker": kind, role: "dialog", "aria-label": `Pick ${kind === "accent" ? "an accent" : "a layout"}` });
      const buttons = choices.map((choice) => {
        const pick = h("button", { type: "button", name: "pick", "data-choice": choice }, choice);
        pick.addEventListener("click", async () => {
          card.remove();
          const result = await this.setTheme({ [kind]: choice });
          resolve({ ...result, picked: choice });
        });
        return pick;
      });
      card.append(h("h3", {}, kind === "accent" ? "Pick an accent" : "Pick a layout"), ...buttons);
      this.parts.dialogs.append(card);
    });
  }
}
