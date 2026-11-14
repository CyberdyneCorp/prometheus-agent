// The desktop's shell commands when the desktop workspace runs in a
// browser (spec: web-dashboard, Desktop workspace in the browser): the
// dashboard listener serves this page under `/app` and its `/api/rpc`
// endpoint is the daemon connection, signed in by the browser's cookie
// and CSRF token. What a browser can do is answered here; the
// native-only features (global shortcuts, always-on-top and HUD windows,
// window reading, the updater, OS file pickers and openers, the tray,
// remote connections) are refused with a reason the page and the agent's
// `desktop_ui` tools report.

import { api, base, profilePath, signIn } from "../../../web/src/lib/api";
import { onSignedOut } from "../../../web/src/lib/events";
import { dashboardDial } from "../../../web/src/lib/rpc";
import type { Dial, Opened } from "./shell";

/** Why a native-only command did not run. */
export const NATIVE_ONLY = "not available in the webapp: it needs the Prometheus desktop app";

/** The profile the window shows: staged files go to its store. */
let shownProfile = "default";

/** Records the profile the window shows. */
export function showProfile(profile: string): void {
  shownProfile = profile;
}

/** The staging answer of the dashboard (`POST stage`). */
interface Staged {
  blob: string;
  bytes: number;
  path: string;
}

/** Stages `file` under `profile` (16 MiB at most; the daemon refuses
 *  a larger one with 413 before anything is stored); the daemon-side
 *  path a prompt attaches. */
export async function stageFile(profile: string, file: File): Promise<string> {
  const staged = await api.upload<Staged>(profilePath(profile, "/stage"), file);
  return staged.path;
}

/** The browser's file picker, the picked file staged. */
function pickAndStage(): Promise<{ path: string | null }> {
  return new Promise((resolve, reject) => {
    const input = document.createElement("input");
    input.type = "file";
    input.addEventListener("change", () => {
      const file = input.files?.[0];
      if (!file) return resolve({ path: null });
      stageFile(shownProfile, file).then((path) => resolve({ path }), reject);
    });
    input.addEventListener("cancel", () => resolve({ path: null }));
    input.click();
  });
}

function openExternal(args: Record<string, unknown>): Opened {
  const url = String(args.url ?? "");
  if (!/^https?:\/\//i.test(url)) return { ok: false, reason: "only http and https links open from the webapp" };
  window.open(url, "_blank", "noopener,noreferrer");
  return { ok: true };
}

async function attach(): Promise<unknown> {
  const listed = await api.get<{ profiles: { name: string }[] }>("/api/profiles");
  return { status: "attached", version: "", profiles: listed.profiles.map((p) => p.name) };
}

function launchOptions(): unknown {
  const notices = window.isSecureContext
    ? []
    : ["This address is plain HTTP: the microphone, clipboard reading, notifications and screen viewing are unavailable here."];
  return { notices, window: "main" };
}

/** The only connection: the daemon serving this page. */
const LISTING = {
  connections: [{ id: "local", name: "This daemon", kind: "local" }],
  primary: "local",
  open_on_launch: "primary",
  launch: "local",
  latched: {},
  paired: [],
};

const nothing = (): null => null;

type Handler = (args: Record<string, unknown>) => unknown;

/** The commands a browser answers; any other one is native-only. */
const HANDLERS: Record<string, Handler> = {
  attach,
  launch_options: launchOptions,
  connections_list: () => LISTING,
  prepare_dial: (): Dial => ({ dial: "webapp", ...dashboardDial() }),
  settle_dial: nothing,
  connection_connected: nothing,
  take_links: () => [],
  background_check: () => ({}),
  open_external: openExternal,
  pick_file: pickAndStage,
  read_clipboard: async () => ({ text: await navigator.clipboard.readText() }),
  new_window: () => void window.open(location.href, "_blank", "noopener"),
  pop_out: (args) => void window.open(`#session=${encodeURIComponent(String(args.profile))}/${encodeURIComponent(String(args.session_id))}`, "_blank", "noopener"),
  notify_approval: nothing,
  request_settled: nothing,
  set_tray: nothing,
  plugin_refused: nothing,
};

/** Runs `command` in the browser. */
export async function webShell<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  const handler = HANDLERS[command];
  if (!handler) throw new Error(NATIVE_ONLY);
  return (await handler(args)) as T;
}


/** The sign-in instructions, in place of the workspace. */
function signedOut(root: HTMLElement): void {
  const heading = document.createElement("h1");
  heading.textContent = "Sign in to Prometheus";
  const how = document.createElement("p");
  how.textContent = "Run prometheus webapp on the machine that serves this workspace to get a one-time sign-in link.";
  const section = document.createElement("section");
  section.className = "signed-out";
  section.append(heading, how);
  root.replaceChildren(section);
}

/** Starts the webapp: `true` once the browser is signed in (its CSRF
 *  token is then known); otherwise the sign-in instructions show. A later
 *  sign-out (`prometheus webapp --sign-out`, or from another tab) closes
 *  the event stream with 4401 (or answers an API call 401) and the page
 *  loads again, showing the instructions. */
export async function startWebapp(root: HTMLElement): Promise<boolean> {
  const session = await signIn().catch(() => null);
  if (!session) {
    signedOut(root);
    return false;
  }
  // A fresh load drops every socket and timer of the workspace and finds
  // the browser signed out.
  const ended = () => location.reload();
  onSignedOut(ended);
  document.addEventListener("signed-out", ended);
  return true;
}

/** Downloads a host file of a session; the daemon applies the session's
 *  file-access rules (`GET sessions/{id}/file`). */
export async function downloadHostFile(profile: string, sessionId: string, path: string): Promise<void> {
  const url = `${base}${profilePath(profile, `/sessions/${encodeURIComponent(sessionId)}/file`)}?path=${encodeURIComponent(path)}`;
  const response = await fetch(url, { credentials: "same-origin" });
  if (!response.ok) {
    const body = (await response.json().catch(() => ({}))) as { message?: string };
    throw new Error(body.message ?? `HTTP ${response.status}`);
  }
  const link = document.createElement("a");
  link.href = URL.createObjectURL(await response.blob());
  link.download = path.split(/[\\/]/).pop() ?? "download";
  link.click();
  URL.revokeObjectURL(link.href);
}
