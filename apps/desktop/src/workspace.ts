// The window around the chat (spec: desktop-app, Windows, tabs and panes;
// Interface modes and layouts; File browser pane; Artifacts gallery and
// suggestions; Preview rail and in-app browser; Embedded terminals are
// daemon-owned PTYs; Live subagents frame): tabs over the chat, the
// terminal under it, the right sidebar's panes, the layout controls and
// the window's keyboard shortcuts.

import { h } from "../../../web/src/lib/dom";
import { ArtifactsView, Suggestions } from "./artifacts";
import type { ChatView } from "./chat";
import type { DesktopClient } from "./client";
import { FileBrowser } from "./files";
import { type Arrangement, Layout, type Mode } from "./layout";
import { PreviewRail } from "./preview";
import { PreviewDriver } from "./preview-driver";
import { ReviewPane } from "./review";
import { shell } from "./shell";
import { Shortcuts } from "./shortcuts";
import { SubagentsFrame } from "./subagents";
import { Tabs } from "./tabs";
import { TerminalPane } from "./terminal";

/** The right sidebar's panes, one shown at a time. */
class Panes {
  readonly tabs = h("nav", { class: "pane-tabs", "aria-label": "Panes" });
  private readonly panes = new Map<string, { el: HTMLElement; shown?: () => void }>();
  onPick: (name: string) => void = () => {};

  add(name: string, label: string, el: HTMLElement, shown?: () => void): HTMLElement {
    const host = h("div", { class: "pane", "data-pane": name, hidden: this.panes.size > 0 }, el);
    this.panes.set(name, { el: host, shown });
    const tab = h("button", { type: "button", name: "pane", "data-pane": name }, label);
    tab.addEventListener("click", () => this.onPick(name));
    this.tabs.append(tab);
    return host;
  }

  show(name: string): void {
    for (const [key, pane] of this.panes) pane.el.hidden = key !== name;
    this.panes.get(name)?.shown?.();
  }
}

/** What the window is made of besides the header and the session list. */
export interface Workspace {
  center: HTMLElement;
  aside: HTMLElement;
  layout: Layout;
  /** Every bindable action of the window. */
  shortcuts: Shortcuts;
  /** Shows a pane of the right sidebar. */
  showPane(name: string): void;
  /** The pane names, for the palette. */
  paneNames: string[];
  /** Lays the window out once it is in the document. */
  start(side: HTMLElement): void;
  /** The parts the desktop UI tools act on. */
  terminal: TerminalPane;
  preview: PreviewRail;
  driver: PreviewDriver;
}

/** A pane the window adds (management panes). */
export interface ExtraPane {
  name: string;
  label: string;
  el: HTMLElement;
  shown?: () => void;
}

export function workspace(client: DesktopClient, chat: ChatView, settings: HTMLElement, dialogs: HTMLElement, extra: ExtraPane[]): Workspace {
  const files = new FileBrowser(client);
  const preview = new PreviewRail(client, (comments) => chat.attachComments(comments.text, comments.images));
  const driver = new PreviewDriver(preview);
  const terminal = new TerminalPane(client, (text) => chat.attachComments(text, []));
  const subagents = new SubagentsFrame(client);
  const review = new ReviewPane(client, dialogs);
  const panes = new Panes();
  const tabs = new Tabs((sessionId) => (sessionId ? void chat.open(sessionId) : chat.draft()));
  let side: HTMLElement | null = null;
  const aside = h("aside", { class: "aside" });
  const apply = (mode: Mode, a: Arrangement) => {
    document.documentElement.dataset.uiMode = mode;
    if (side) side.hidden = !a.left;
    aside.hidden = !a.right;
    terminal.el.hidden = !a.terminal;
    panes.show(a.pane);
  };
  const layout = new Layout(apply);
  panes.onPick = (name) => layout.change({ pane: name, right: true });
  const actions = {
    openSession: (id: string) => void chat.open(id),
    preview: (target: Parameters<PreviewRail["open"]>[0]) => {
      layout.change({ pane: "preview", right: true });
      void preview.open(target);
    },
  };
  const artifacts = new ArtifactsView(client, actions);
  const suggestions = new Suggestions(client, actions);
  chat.aboveComposer(suggestions.el);
  chat.aboveComposer(subagents.el);
  chat.transcript.onReveal = (path) => {
    layout.change({ pane: "files", right: true });
    void files.reveal(path);
  };
  chat.onFollow = (sessionId) => {
    // Pins belong to the chat they were made in; each chat has its frame.
    preview.annotate.clear();
    tabs.showing(sessionId);
    subagents.follow(sessionId);
    void files.follow(sessionId);
    void suggestions.follow(sessionId);
    void terminal.follow(sessionId);
    void review.follow(sessionId);
  };
  const popOut = h("button", { type: "button", name: "pop-out" }, "Pop out");
  popOut.addEventListener("click", () => {
    if (chat.sessionId) void shell("pop_out", { profile: client.profile, session_id: chat.sessionId });
  });
  aside.append(
    panes.tabs,
    panes.add("settings", "Settings", settings),
    panes.add("files", "Files", files.el),
    panes.add("artifacts", "Artifacts", artifacts.el, () => void artifacts.load()),
    panes.add("preview", "Preview", preview.el, () => preview.show()),
    panes.add("review", "Review", review.el, () => void review.load()),
    ...extra.map((p) => panes.add(p.name, p.label, p.el, p.shown)),
  );
  const center = h("div", { class: "center" }, h("div", { class: "center-bar" }, tabs.el, popOut), chat.el, terminal.el);
  const keys = shortcuts(chat, { tabs, terminal, layout });
  document.addEventListener("keydown", (event) => keys.handle(event));
  return {
    center,
    aside,
    layout,
    shortcuts: keys,
    showPane: (name) => layout.change({ pane: name, right: true }),
    paneNames: ["settings", "files", "artifacts", "preview", "review", ...extra.map((p) => p.name)],
    start: (sideEl) => {
      side = sideEl;
      layout.start();
    },
    terminal,
    preview,
    driver,
  };
}

interface Parts {
  tabs: Tabs;
  terminal: TerminalPane;
  layout: Layout;
}

/** The window's bindable actions (they work in every mode). */
function shortcuts(chat: ChatView, p: Parts): Shortcuts {
  const keys = new Shortcuts();
  const toggleReview = () => {
    const shown = p.layout.arrangement.right && p.layout.arrangement.pane === "review";
    p.layout.change(shown ? { right: false } : { pane: "review", right: true });
  };
  const actions: [string, string, string, () => void][] = [
    ["find", "Find in chat", "CommandOrControl+F", () => chat.find.open()],
    ["status-bar", "Status bar details", "CommandOrControl+Shift+S", () => chat.status.toggle()],
    ["new-window", "New window", "CommandOrControl+Shift+N", () => void shell("new_window")],
    ["new-session", "New session", "CommandOrControl+N", () => chat.draft()],
    ["new-tab", "New tab", "CommandOrControl+T", () => p.tabs.open()],
    ["reopen-tab", "Reopen closed tab", "CommandOrControl+Shift+T", () => p.tabs.reopen()],
    ["close-tab", "Close tab", "CommandOrControl+W", () => p.tabs.close()],
    ["next-tab", "Next tab", "Ctrl+Tab", () => p.tabs.cycle(false)],
    ["previous-tab", "Previous tab", "Ctrl+Shift+Tab", () => p.tabs.cycle(true)],
    ["left-sidebar", "Toggle left sidebar", "CommandOrControl+B", () => p.layout.change({ left: !p.layout.arrangement.left })],
    ["right-sidebar", "Toggle right sidebar", "CommandOrControl+J", () => p.layout.change({ right: !p.layout.arrangement.right })],
    ["review", "Toggle the review pane", "CommandOrControl+G", toggleReview],
  ];
  for (const [id, label, chord, run] of actions) keys.add({ id, label, chord, run });
  // Over an interactive terminal, the shell gets CommandOrControl+W.
  keys.skip = (id) => id === "close-tab" && p.terminal.focused;
  return keys;
}
