// The desktop window (spec: desktop-app): attaches through the shell,
// then renders sessions, chat and settings over the window's protocol
// client. Every profile-data request names the window's profile, and a
// profile switch detaches and redials before anything of the new profile
// is shown. Display preferences are the machine's (the local daemon's
// `ui.prefs`), approvals for an unfocused window become notifications,
// and plugin panels load on their own origins.

import type { ApprovalRequestParams } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import { Prefs } from "../../../web/src/lib/prefs";
import { ArtifactsView, Suggestions } from "./artifacts";
import { Attention, focusCard } from "./attention";
import { ChatView } from "./chat";
import { DesktopClient } from "./client";
import { desktopSettings } from "./desktop-settings";
import { FileBrowser } from "./files";
import { type InstallLink, installDialog } from "./install";
import { PanelHost } from "./panels";
import { PreviewRail } from "./preview";
import { LAST_PROFILE } from "./quickask";
import { SettingsPanel } from "./settings";
import { type Attach, onShellEvent, shell } from "./shell";
import { NOT_FOUND } from "./verdict";

interface SessionSummary {
  session_id: string;
  title?: string | null;
}

type DeepLink = { open: "session"; session_id: string } | { open: "new"; profile?: string; cwd?: string } | InstallLink;

interface Launch {
  notices: string[];
  /** This window's label (drafts are kept per window). */
  window?: string;
}

/** How often the window asks for an app update (the shell checks the
 *  channel at most every 6 hours). */
const UPDATE_CHECK_MS = 60 * 60 * 1000;

const root = (): HTMLElement => document.getElementById("app") as HTMLElement;
const notices = h("div", { class: "notices", "aria-live": "polite" });

function say(text: string, kind: "info" | "warn" | "error" = "info"): void {
  notices.append(notice(text, kind));
}

/** Attaches to the local daemon (asking before restarting a busy stale
 *  one) and opens the window, or shows why not with Retry. */
export async function boot(restart = false): Promise<void> {
  root().replaceChildren(h("p", { class: "boot", role: "status" }, "Starting Prometheus…"));
  const outcome = await shell<Attach>(restart ? "restart_daemon" : "attach", {}).catch((err: unknown): Attach => ({ status: "failed", error: String(err) }));
  if (outcome.status === "failed") return failed(outcome.error, outcome.log);
  if (outcome.status === "confirm_restart") return confirmRestart(outcome);
  if (outcome.status === "no_local") return noLocal();
  if (outcome.notice) say(outcome.notice, "warn");
  const launch = await shell<Launch>("launch_options").catch((): Launch => ({ notices: [] }));
  window_(outcome.profiles, launch.window ?? "main");
}

function failed(error: string, log?: string): void {
  const retry = h("button", { type: "button", name: "retry" }, "Retry");
  retry.addEventListener("click", () => void boot());
  const logLink = log ? h("p", {}, "See the daemon log: ", h("a", { href: log, class: "log-link" }, log)) : null;
  root().replaceChildren(h("section", { class: "boot-failed", role: "alert" }, h("h1", {}, "Prometheus could not start"), h("p", { class: "error" }, error), logLink, retry));
}

function noLocal(): void {
  root().replaceChildren(
    h(
      "section",
      { class: "boot-failed", role: "status" },
      h("h1", {}, "No local daemon"),
      h("p", {}, "The app was started with --ignore-existing, so it did not start a gateway daemon on this machine. Start it without the flag to use the local agent."),
    ),
  );
}

function confirmRestart(outcome: { running_turns: number; daemon: string; bundled: string }): void {
  const restart = h("button", { type: "button", name: "restart" }, "Restart now");
  restart.addEventListener("click", () => void boot(true));
  const later = h("button", { type: "button", name: "later" }, "Not now");
  later.addEventListener("click", () => window_([], "main"));
  root().replaceChildren(
    h(
      "section",
      { class: "confirm-restart", role: "alertdialog" },
      h("p", {}, `The running daemon (${outcome.daemon}) is not this app's build (${outcome.bundled}) and ${outcome.running_turns} turn(s) are running. Restart it now?`),
      restart,
      later,
    ),
  );
}

interface Opener {
  chat: ChatView;
  client: DesktopClient;
  switchTo: (profile: string) => Promise<void>;
  profiles: string[];
  dialogs: HTMLElement;
}

function window_(profiles: string[], label: string): void {
  const served = profiles.length > 0 ? profiles : ["default"];
  // The default profile first, as the CLI does without `-p`.
  const client = new DesktopClient(served.includes("default") ? "default" : (served[0] ?? "default"));
  const dialogs = h("div", { class: "dialogs" });
  const chat = new ChatView({ client, window: label, dialogs });
  const settings = new SettingsPanel(client, served);
  const files = new FileBrowser(client);
  const preview = new PreviewRail(client, (comments) => chat.attachComments(comments.text, comments.images));
  const panes = new Panes();
  const actions = {
    openSession: (id: string) => void chat.open(id),
    preview: (target: Parameters<PreviewRail["open"]>[0]) => {
      panes.show("preview");
      void preview.open(target);
    },
  };
  const artifacts = new ArtifactsView(client, actions);
  const suggestions = new Suggestions(client, actions);
  chat.aboveComposer(suggestions.el);
  chat.transcript.onReveal = (path) => {
    panes.show("files");
    void files.reveal(path);
  };
  chat.onFollow = (sessionId) => {
    // Pins belong to the chat they were made in.
    preview.annotate.clear();
    void files.follow(sessionId);
    void suggestions.follow(sessionId);
  };
  const attention = new Attention();
  const panels = new PanelHost(client);
  const prefs = new Prefs(client);
  const status = h("span", { class: "connection", role: "status" }, "Connecting…");
  const list = h("ul", { class: "sessions", "aria-label": "Sessions" });
  const picker = h("select", { name: "profile", "aria-label": "Profile" }, ...served.map((p) => h("option", { value: p, selected: p === "default" }, p)));
  const desktopSection = h("section", { class: "desktop-settings", "aria-label": "Desktop settings" });

  const refresh = async () => {
    const listed = await client.call<{ sessions: SessionSummary[] }>("session.list", { profile: client.profile }).catch(() => ({ sessions: [] }));
    list.replaceChildren(
      ...listed.sessions.map((s) => {
        const button = h("button", { type: "button", "data-session": s.session_id }, s.title ?? s.session_id);
        button.addEventListener("click", () => void chat.open(s.session_id));
        return h("li", {}, button);
      }),
    );
  };
  const switchTo = async (profile: string) => {
    if (profile === client.profile) return;
    chat.draft();
    list.replaceChildren();
    picker.value = profile;
    remember(profile);
    await client.switchProfile(profile);
  };

  client.onEvent = (envelope) => chat.apply(envelope);
  client.onRequest = (request) => {
    chat.request(request);
    if (request.method === "approval.request") attention.approval(request.id, request.params as unknown as ApprovalRequestParams, chat.title());
  };
  for (const event of ["request.resolved", "request.cancel"]) client.on(event, (envelope) => attention.settled((envelope.data as { id: string }).id));
  client.on("turn.finished", (envelope) => {
    attention.turn(envelope.session_id ?? "", false, "");
    if (envelope.session_id) void chat.turnFinished(envelope.session_id);
  });
  chat.onSubmitted = (sessionId) => attention.turn(sessionId, true, chat.title() ?? sessionId);
  client.onTruncated = (sessionId) => {
    if (sessionId === chat.sessionId) void chat.reload();
  };
  client.onState = (state) => {
    status.textContent = state === "connected" ? `Connected · ${client.profile}` : state === "reconnecting" ? "Reconnecting…" : "Connecting…";
    status.dataset.state = state;
    if (state !== "connected") return;
    void refresh();
    void settings.reset();
    void prefs.start({}).catch((err: unknown) => console.warn("UI preferences unavailable:", err));
    void panels.load(client.profile);
    void desktopSettings(client, desktopSection).catch((err: unknown) => console.warn("desktop settings:", err));
  };
  client.onReattached = () => void chat.reattached();
  picker.addEventListener("change", () => void switchTo(picker.value));

  const newChat = h("button", { type: "button", name: "new-chat" }, "New chat");
  newChat.addEventListener("click", () => chat.draft());
  const botChat = h("button", { type: "button", name: "bot-chat" }, "Bot chat");
  botChat.addEventListener("click", async () => {
    // A click during a daemon restart waits for the redial: the daemon's
    // get-or-create makes every window land on the same chat.
    try {
      const chatRef = await client.callAcrossRestart<{ session_id: string }>("bot.chat.ensure", { profile: client.profile });
      await chat.open(chatRef.session_id);
    } catch (err) {
      say(`Could not open the bot chat: ${(err as Error).message}`, "error");
    }
  });

  const opener: Opener = { chat, client, switchTo, profiles: served, dialogs };
  listenToShell(opener);
  root().replaceChildren(
    h("header", { class: "top" }, h("span", { class: "brand" }, "Prometheus"), picker, status),
    h(
      "div",
      { class: "layout" },
      h("nav", { class: "side" }, newChat, botChat, list),
      chat.el,
      h("aside", { class: "aside" }, panes.tabs, panes.add("settings", "Settings", h("div", {}, settings.el, desktopSection)), panes.add("files", "Files", files.el), panes.add("artifacts", "Artifacts", artifacts.el, () => void artifacts.load()), panes.add("preview", "Preview", preview.el, () => preview.show()), panels.el),
    ),
    dialogs,
    notices,
  );
  remember(client.profile);
  shortcuts(chat);
  chat.draft();
  client.start();
  void afterReady(opener);
}

/** The right sidebar's panes, one shown at a time. */
class Panes {
  readonly tabs = h("nav", { class: "pane-tabs", "aria-label": "Panes" });
  private readonly panes = new Map<string, { el: HTMLElement; shown?: () => void }>();

  add(name: string, label: string, el: HTMLElement, shown?: () => void): HTMLElement {
    const host = h("div", { class: "pane", "data-pane": name, hidden: this.panes.size > 0 }, el);
    this.panes.set(name, { el: host, shown });
    const tab = h("button", { type: "button", name: "pane", "data-pane": name }, label);
    tab.addEventListener("click", () => this.show(name));
    this.tabs.append(tab);
    return host;
  }

  show(name: string): void {
    for (const [key, pane] of this.panes) pane.el.hidden = key !== name;
    this.panes.get(name)?.shown?.();
  }
}

/** The window's keyboard shortcuts. */
function shortcuts(chat: ChatView): void {
  document.addEventListener("keydown", (event) => {
    const mod = event.metaKey || event.ctrlKey;
    if (!mod) return;
    const key = event.key.toLowerCase();
    if (key === "f" && !event.shiftKey) {
      event.preventDefault();
      chat.find.open();
    } else if (key === "s" && event.shiftKey) {
      event.preventDefault();
      chat.status.toggle();
    }
  });
}

/** The shell's events: deep links, focus requests after a notification
 *  click, sessions opened from the tray or the quick-ask panel. */
function listenToShell(o: Opener): void {
  onShellEvent("deep-link", (payload) => void deepLink(payload as DeepLink, o));
  onShellEvent("focus-request", (payload) => void focusRequest(payload as { request_id: string; session_id: string }, o));
  onShellEvent("open-session", (payload) => {
    const asked = payload as { profile?: string; session_id: string };
    void (async () => {
      if (asked.profile && o.profiles.includes(asked.profile)) await o.switchTo(asked.profile);
      await openOwned(asked.session_id, o);
    })();
  });
}

/** What waits for the window: held deep links, launch notices, updates. */
async function afterReady(o: Opener): Promise<void> {
  const launch = await shell<Launch>("launch_options").catch((): Launch => ({ notices: [] }));
  for (const line of launch.notices) say(line, "warn");
  const held = await shell<DeepLink[]>("take_links").catch(() => []);
  for (const link of held) await deepLink(link, o);
  const checkUpdates = async () => {
    const outcome = await shell<{ available?: { version: string } }>("background_check").catch(() => ({}) as { available?: { version: string } });
    if (outcome.available) say(`Prometheus ${outcome.available.version} is available: Settings → Updates.`);
  };
  void checkUpdates();
  window.setInterval(() => void checkUpdates(), UPDATE_CHECK_MS);
}

function remember(profile: string): void {
  try {
    localStorage.setItem(LAST_PROFILE, profile);
  } catch {
    // The quick-ask panel then asks the default profile.
  }
}

/** A notification was clicked: show its session and focus its card. The
 *  request stays pending until the user answers it in the card. */
async function focusRequest(asked: { request_id: string; session_id: string }, o: Opener): Promise<void> {
  if (o.chat.sessionId !== asked.session_id) await openOwned(asked.session_id, o);
  if (!focusCard(asked.request_id)) say("That approval was already answered or settled.", "info");
}

/** Opens `sessionId` in the profile that owns it: the window's first,
 *  then the other served profiles. */
async function openOwned(sessionId: string, o: Opener): Promise<void> {
  const first = o.client.profile;
  for (const profile of [first, ...o.profiles.filter((p) => p !== first)]) {
    await o.switchTo(profile);
    try {
      await o.client.call("session.attach", { session_id: sessionId, role: "observer" });
      await o.client.call("session.detach", { session_id: sessionId });
      await o.chat.open(sessionId);
      return;
    } catch (err) {
      if ((err as { code?: number }).code !== NOT_FOUND) break;
    }
  }
  await o.switchTo(first);
  await o.chat.open(sessionId);
}

/** What a deep link opens; it never submits input or answers a request,
 *  and an install link only opens the consent dialog. */
async function deepLink(link: DeepLink, o: Opener): Promise<void> {
  switch (link.open) {
    case "new":
      if (link.profile && o.profiles.includes(link.profile)) await o.switchTo(link.profile);
      o.chat.draft(link.cwd ?? null);
      return;
    case "session":
      await openOwned(link.session_id, o);
      return;
    default:
      await installDialog(link, o.client.profile, o.dialogs);
  }
}
