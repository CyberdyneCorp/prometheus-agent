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
import { shell } from "./shell";
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
  /** Lays the window out once it is in the document. */
  start(side: HTMLElement): void;
}

export function workspace(client: DesktopClient, chat: ChatView, settings: HTMLElement): Workspace {
  const files = new FileBrowser(client);
  const preview = new PreviewRail(client, (comments) => chat.attachComments(comments.text, comments.images));
  const terminal = new TerminalPane(client, (text) => chat.attachComments(text, []));
  const subagents = new SubagentsFrame(client);
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
  );
  const center = h("div", { class: "center" }, h("div", { class: "center-bar" }, tabs.el, popOut), chat.el, terminal.el);
  const keys = shortcuts(chat, { tabs, terminal, layout });
  document.addEventListener("keydown", keys);
  return {
    center,
    aside,
    layout,
    start: (sideEl) => {
      side = sideEl;
      layout.start();
    },
  };
}

interface Parts {
  tabs: Tabs;
  terminal: TerminalPane;
  layout: Layout;
}

/** The window's keyboard shortcuts (they work in every mode). */
function shortcuts(chat: ChatView, p: Parts): (event: KeyboardEvent) => void {
  const table: Record<string, () => void> = {
    f: () => chat.find.open(),
    "shift+s": () => chat.status.toggle(),
    "shift+n": () => void shell("new_window"),
    t: () => p.tabs.open(),
    "shift+t": () => p.tabs.reopen(),
    w: () => p.tabs.close(),
    b: () => p.layout.change({ left: !p.layout.arrangement.left }),
    j: () => p.layout.change({ right: !p.layout.arrangement.right }),
  };
  return (event) => {
    if (event.ctrlKey && event.key === "Tab") {
      event.preventDefault();
      p.tabs.cycle(event.shiftKey);
      return;
    }
    if (!(event.metaKey || event.ctrlKey)) return;
    const key = `${event.shiftKey ? "shift+" : ""}${event.key.toLowerCase()}`;
    // Over an interactive terminal, the shell gets CommandOrControl+W.
    if (key === "w" && p.terminal.focused) return;
    const run = table[key];
    if (!run) return;
    event.preventDefault();
    run();
  };
}
