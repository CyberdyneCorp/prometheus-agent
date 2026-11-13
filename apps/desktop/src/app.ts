// The desktop window (spec: desktop-app): attaches through the shell,
// then renders sessions, chat and settings over the window's protocol
// client. Every profile-data request names the window's profile, and a
// profile switch detaches and redials before anything of the new profile
// is shown.

import { h, notice } from "../../../web/src/lib/dom";
import { ChatView } from "./chat";
import { DesktopClient } from "./client";
import { NOT_FOUND } from "./verdict";
import { settingsPanel } from "./settings";
import { type Attach, onShellEvent, shell } from "./shell";

interface SessionSummary {
  session_id: string;
  title?: string | null;
}

type DeepLink =
  | { open: "session"; session_id: string }
  | { open: "new"; profile?: string; cwd?: string }
  | { open: "install_plugin" | "install_skill"; id: string };

const root = (): HTMLElement => document.getElementById("app") as HTMLElement;
const notices = h("div", { class: "notices", "aria-live": "polite" });

function say(text: string, kind: "info" | "warn" | "error" = "info"): void {
  notices.append(notice(text, kind));
}

/** Attaches to the local daemon (asking before restarting a busy stale
 *  one) and opens the window, or shows why not with Retry. */
export async function boot(restart = false): Promise<void> {
  root().replaceChildren(h("p", { class: "boot", role: "status" }, "Starting Prometheus…"));
  const outcome = await shell<Attach>(restart ? "restart_daemon" : "attach", {}).catch(
    (err: unknown): Attach => ({ status: "failed", error: String(err) }),
  );
  if (outcome.status === "failed") return failed(outcome.error, outcome.log);
  if (outcome.status === "confirm_restart") return confirmRestart(outcome);
  if (outcome.notice) say(outcome.notice, "warn");
  window_(outcome.profiles);
}

function failed(error: string, log?: string): void {
  const retry = h("button", { type: "button", name: "retry" }, "Retry");
  retry.addEventListener("click", () => void boot());
  const logLink = log ? h("p", {}, "See the daemon log: ", h("a", { href: log, class: "log-link" }, log)) : null;
  root().replaceChildren(h("section", { class: "boot-failed", role: "alert" }, h("h1", {}, "Prometheus could not start"), h("p", { class: "error" }, error), logLink, retry));
}

function confirmRestart(outcome: { running_turns: number; daemon: string; bundled: string }): void {
  const restart = h("button", { type: "button", name: "restart" }, "Restart now");
  restart.addEventListener("click", () => void boot(true));
  const later = h("button", { type: "button", name: "later" }, "Not now");
  later.addEventListener("click", () => window_([]));
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

function window_(profiles: string[]): void {
  const served = profiles.length > 0 ? profiles : ["default"];
  // The default profile first, as the CLI does without `-p`.
  const client = new DesktopClient(served.includes("default") ? "default" : (served[0] ?? "default"));
  const chat = new ChatView(client);
  const status = h("span", { class: "connection", role: "status" }, "Connecting…");
  const list = h("ul", { class: "sessions", "aria-label": "Sessions" });
  const picker = h("select", { name: "profile", "aria-label": "Profile" }, ...served.map((p) => h("option", { value: p, selected: p === "default" }, p)));

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
    await client.switchProfile(profile);
  };

  client.onEvent = (envelope) => chat.apply(envelope);
  client.onRequest = (request) => chat.request(request);
  client.onTruncated = (sessionId) => {
    if (sessionId === chat.sessionId) void chat.reload();
  };
  client.onState = (state) => {
    status.textContent = state === "connected" ? `Connected · ${client.profile}` : state === "reconnecting" ? "Reconnecting…" : "Connecting…";
    status.dataset.state = state;
    if (state === "connected") void refresh();
  };
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

  const opener: Opener = { chat, client, switchTo, profiles: served };
  onShellEvent("deep-link", (payload) => void deepLink(payload as DeepLink, opener));
  root().replaceChildren(
    h("header", { class: "top" }, h("span", { class: "brand" }, "Prometheus"), picker, status),
    h("div", { class: "layout" }, h("nav", { class: "side" }, newChat, botChat, list), chat.el, settingsPanel(client)),
    notices,
  );
  client.start();
}

interface Opener {
  chat: ChatView;
  client: DesktopClient;
  switchTo: (profile: string) => Promise<void>;
  profiles: string[];
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

/** What a deep link opens; it never submits input or answers a request. */
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
      say(`Installing ${link.id} from a link needs the catalog install dialog, which this build does not have yet.`, "warn");
  }
}
