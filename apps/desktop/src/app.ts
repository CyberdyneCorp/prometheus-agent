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
import { applyFonts, restore, snapshot, Zoom } from "./appearance";
import { Attention, focusCard } from "./attention";
import { ChatView } from "./chat";
import { DesktopClient } from "./client";
import { CommandPalette, type Entry } from "./command-palette";
import { desktopSettings } from "./desktop-settings";
import { MemoryGraph } from "./graph";
import { hudWindow, toggleHud } from "./hud";
import { type InstallLink, installDialog } from "./install";
import { ManagePane } from "./manage";
import { PanelHost } from "./panels";
import { ProfileClients } from "./profiles";
import { ProjectsSidebar } from "./projects";
import { LAST_PROFILE } from "./quickask";
import { SettingsPanel } from "./settings";
import { type Attach, onShellEvent, shell } from "./shell";
import { NOT_FOUND } from "./verdict";
import { type Workspace, workspace } from "./workspace";

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
  const pool = new ProfileClients(client);
  const settings = new SettingsPanel(client, served, pool);
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
    manage.reset();
    void prefs.start({}).catch((err: unknown) => console.warn("UI preferences unavailable:", err));
    void panels.load(client.profile);
    void projects.load();
    void applyFonts(client);
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
  const graph = new MemoryGraph(client);
  chat.onJourney = () => void graph.open();
  const projects = new ProjectsSidebar(client, (path) => chat.draft(path));
  const zoom = new Zoom();
  const files = profileFiles(prefs, zoom, say);
  const manage = new ManagePane(client, pool, served, dialogs, {
    openSession: async (profile, sessionId) => {
      if (served.includes(profile)) await switchTo(profile);
      await openOwned(sessionId, opener);
    },
    switchProfile: switchTo,
    exportProfile: files.exportProfile,
    importProfile: files.importProfile,
  });
  const work = workspace(client, chat, h("div", {}, settings.el, desktopSection), dialogs, [{ name: "manage", label: "Manage", el: manage.el, shown: () => manage.reset() }]);
  const keys = work.shortcuts;
  manage.setShortcuts(keys.panel);
  keys.onSave = (overrides) => void prefs.set("keybindings", overrides).catch(() => {});
  prefs.onChange((values) => keys.load(values.keybindings));
  panels.onShortcut = (shortcut) => keys.add(shortcut);
  const palette = new CommandPalette(() => paletteEntries(opener, work, { graph, files, zoom }));
  windowActions(keys, opener, { palette, graph, zoom });
  work.aside.append(panels.el);
  const side = h("nav", { class: "side" }, newChat, botChat, list, projects.el);
  listenToShell(opener);
  onShellEvent("hud-summon", () => void toggleHud(client.profile, chat.sessionId));
  root().replaceChildren(
    h("header", { class: "top" }, h("span", { class: "brand" }, "Prometheus"), picker, work.layout.el, status),
    h(
      "div",
      { class: "layout" },
      side,
      work.center,
      work.aside,
    ),
    dialogs,
    notices,
    graph.el,
    palette.el,
  );
  remember(client.profile);
  work.start(side);
  chat.draft();
  void poppedOut(opener, side, work.center);
  client.start();
  void afterReady(opener);
}

/** A popped-out window (`#session=<profile>/<id>`) or the HUD bar
 *  (`#hud=<profile>/<id>`): that session only, without the sidebar and
 *  tabs. */
async function poppedOut(o: Opener, side: HTMLElement, center: HTMLElement): Promise<void> {
  const match = /^#(session|hud)=([^/]+)\/(.+)$/.exec(location.hash);
  if (!match) return;
  const [, kind = "", profile = "", sessionId = ""] = match;
  side.remove();
  center.querySelector(".center-bar")?.remove();
  document.documentElement.dataset.window = "pop-out";
  if (kind === "hud") hudWindow(o.chat, center);
  await o.client.ready();
  if (o.profiles.includes(profile)) await o.switchTo(profile);
  await o.chat.open(sessionId);
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

interface Extras {
  palette: CommandPalette;
  graph: MemoryGraph;
  zoom: Zoom;
}

/** The window's actions beyond the workspace's: the palette, the HUD,
 *  the memory graph and zoom. */
function windowActions(keys: Workspace["shortcuts"], o: Opener, x: Extras): void {
  const actions: [string, string, string, () => void][] = [
    ["palette", "Command palette", "CommandOrControl+K", () => void x.palette.open()],
    ["palette-p", "Command palette (alternative)", "CommandOrControl+P", () => void x.palette.open()],
    ["hud", "HUD mode", "CommandOrControl+Shift+H", () => void toggleHud(hudProfile(o), hudSession(o))],
    ["memory-graph", "Memory graph", "CommandOrControl+Shift+M", () => void x.graph.open()],
    ["zoom-in", "Zoom in", "CommandOrControl+=", () => x.zoom.step(1)],
    ["zoom-out", "Zoom out", "CommandOrControl+-", () => x.zoom.step(-1)],
    ["zoom-reset", "Actual size", "CommandOrControl+0", () => x.zoom.step(0)],
  ];
  for (const [id, label, chord, run] of actions) keys.add({ id, label, chord, run });
}

/** The HUD bar toggles its own session; the main window its chat's. */
function hudProfile(o: Opener): string {
  return /^#hud=([^/]+)\//.exec(location.hash)?.[1] ?? o.client.profile;
}

function hudSession(o: Opener): string | null {
  return /^#hud=[^/]+\/(.+)$/.exec(location.hash)?.[1] ?? o.chat.sessionId;
}

interface ProfileFiles {
  exportProfile: (profile: string) => Promise<void>;
  importProfile: () => Promise<void>;
}

/** Export through `profile export` (never with secrets), with the
 *  appearance as the overlay; an import applies the overlay it carries. */
function profileFiles(prefs: Prefs, zoom: Zoom, tell: typeof say): ProfileFiles {
  return {
    exportProfile: async (profile) => {
      try {
        const done = await shell<{ path?: string; cancelled?: boolean }>("profile_export", { profile, appearance: snapshot(prefs) });
        if (done.path) tell(`Exported profile ${profile} to ${done.path} (no secrets included).`);
      } catch (err) {
        tell(`Export failed: ${String(err)}`, "error");
      }
    },
    importProfile: async () => {
      try {
        const done = await shell<{ profile?: string; appearance?: unknown; cancelled?: boolean }>("profile_import");
        if (!done.profile) return;
        await restore(prefs, zoom, done.appearance);
        tell(`Imported profile ${done.profile}; restart the app to serve it.`);
      } catch (err) {
        tell(`Import failed: ${String(err)}`, "error");
      }
    },
  };
}

interface PaletteParts {
  graph: MemoryGraph;
  files: ProfileFiles;
  zoom: Zoom;
}

/** What the palette finds: pages, settings, sessions and actions. */
async function paletteEntries(o: Opener, work: Workspace, p: PaletteParts): Promise<Entry[]> {
  const pages: Entry[] = work.paneNames.map((name) => ({ kind: "page", label: name, run: () => work.showPane(name) }));
  const settings: Entry[] = ["Model → Subagents", "General", "Keyboard shortcuts", "HUD"].map((label) => ({ kind: "setting", label, run: () => work.showPane(label === "Keyboard shortcuts" ? "manage" : "settings") }));
  const listed = await o.client.call<{ sessions: SessionSummary[] }>("session.list", { profile: o.client.profile }).catch(() => ({ sessions: [] as SessionSummary[] }));
  const sessions: Entry[] = listed.sessions.map((s) => ({ kind: "session", label: s.title ?? s.session_id, keywords: s.session_id, run: () => void o.chat.open(s.session_id) }));
  const actions: Entry[] = [
    { kind: "action", label: "New chat", run: () => o.chat.draft() },
    { kind: "action", label: "Memory graph", run: () => void p.graph.open() },
    { kind: "action", label: "HUD mode", run: () => void toggleHud(o.client.profile, o.chat.sessionId) },
    { kind: "action", label: "Zoom in", run: () => p.zoom.step(1) },
    { kind: "action", label: "Zoom out", run: () => p.zoom.step(-1) },
    { kind: "action", label: "Import profile…", run: () => void p.files.importProfile() },
    ...o.profiles.map((profile): Entry => ({ kind: "action", label: `Export profile ${profile}`, run: () => void p.files.exportProfile(profile) })),
  ];
  return [...pages, ...settings, ...sessions, ...actions];
}
