// `/app`: the desktop workspace in the browser (spec: web-dashboard,
// Desktop workspace in the browser; Webapp file staging and terminals),
// served when `webapp.enabled = true`. It composes the shared pages
// (sessions, chat, scheduled jobs) with file staging: every file selected,
// pasted or dropped is staged under the addressed profile (16 MiB each,
// `POST stage`) before it enters the attachment flow. Host files download
// only through the session's file-access rules (`GET sessions/{id}/file`,
// checked by the daemon), and URL previews load in a sandboxed frame that
// may run scripts but never open popups, navigate the workspace or reach
// its origin. Native-only features (global shortcuts, always-on-top
// windows, window reading, the updater) are absent here.

import { api, ApiError, base, profilePath } from "../lib/api";
import { h, notice } from "../lib/dom";
import { canDrive, profileOf } from "../lib/state";
import { cronPage } from "./catalog";
import { chatPage } from "./chat";
import { sessionsPage } from "./sessions";

/** The largest staged file. */
const MAX_STAGED = 16 * 1024 * 1024;

interface Staged {
  blob: string;
  bytes: number;
  path: string;
}

function staging(profile: string): HTMLElement {
  const list = h("ul", { class: "staged", "aria-label": "Staged files" });
  const status = h("p", { class: "muted", role: "status" });
  const stage = async (file: File) => {
    if (file.size > MAX_STAGED) {
      status.textContent = `${file.name} is larger than 16 MiB and was not staged`;
      return;
    }
    try {
      const staged = await api.upload<Staged>(profilePath(profile, "/stage"), file);
      list.append(h("li", { "data-blob": staged.blob }, `${file.name} (${staged.bytes} bytes)`));
      status.textContent = `Staged ${file.name}`;
    } catch (err) {
      status.textContent = err instanceof ApiError ? err.message : String(err);
    }
  };
  const input = h("input", { type: "file", multiple: true, "aria-label": "Stage files", disabled: !canDrive() });
  input.addEventListener("change", () => {
    for (const file of input.files ?? []) void stage(file);
  });
  const zone = h("div", { class: "dropzone", "data-dropzone": "staging" }, "Drop or paste files here to stage them", input);
  zone.addEventListener("dragover", (event) => event.preventDefault());
  zone.addEventListener("drop", (event) => {
    event.preventDefault();
    for (const file of event.dataTransfer?.files ?? []) void stage(file);
  });
  zone.addEventListener("paste", (event) => {
    for (const file of event.clipboardData?.files ?? []) void stage(file);
  });
  return h("section", { class: "pane", "aria-label": "Files" }, h("h2", {}, "Files"), zone, status, list);
}

/** The sandbox of preview frames: scripts and forms, never popups,
 *  top-level navigation or the workspace's origin. */
export const PREVIEW_SANDBOX = "allow-scripts allow-forms";

function preview(): HTMLElement {
  const frame = h("iframe", {
    class: "preview",
    title: "Preview",
    sandbox: PREVIEW_SANDBOX,
    referrerpolicy: "no-referrer",
    hidden: true,
  });
  const status = h("p", { class: "muted", role: "status" });
  const address = h("input", { type: "url", placeholder: "http://localhost:5173", "aria-label": "Preview URL" });
  const form = h("form", { class: "row" }, address, h("button", { type: "submit" }, "Preview"));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    let url: URL;
    try {
      url = new URL(address.value);
    } catch {
      status.textContent = "Enter a full http:// or https:// address";
      return;
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      status.textContent = "Only http:// and https:// addresses preview";
      return;
    }
    frame.src = url.href;
    frame.hidden = false;
    status.textContent = `Previewing ${url.href}`;
  });
  return h("section", { class: "pane wide", "aria-label": "Preview" }, h("h2", {}, "Preview"), form, status, frame);
}

function hostFiles(profile: string): HTMLElement {
  const session = h("input", { placeholder: "ses_...", "aria-label": "Session" });
  const path = h("input", { placeholder: "src/main.rs", "aria-label": "Host file path" });
  const status = h("p", { class: "muted", role: "status" });
  const form = h("form", { class: "row" }, session, path, h("button", { type: "submit" }, "Download"));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const url = `${base}${profilePath(profile, `/sessions/${encodeURIComponent(session.value)}/file`)}?path=${encodeURIComponent(path.value)}`;
    void (async () => {
      const response = await fetch(url, { credentials: "same-origin" });
      if (!response.ok) {
        const body = (await response.json().catch(() => ({}))) as { message?: string };
        status.textContent = body.message ?? `HTTP ${response.status}`;
        return;
      }
      const link = h("a", { href: URL.createObjectURL(await response.blob()), download: path.value.split(/[\\/]/).pop() ?? "download" });
      link.click();
      URL.revokeObjectURL(link.href);
      status.textContent = `Downloaded ${path.value}`;
    })();
  });
  return h("section", { class: "pane", "aria-label": "Host files" }, h("h2", {}, "Host files"), form, status);
}

export async function workspacePage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const [sessions, chat, cron] = await Promise.all([sessionsPage(params), chatPage(params), cronPage(params)]);
  return h(
    "section",
    { class: "page workspace", "aria-label": "Workspace" },
    h("h1", {}, "Workspace"),
    notice("Running in the browser: global shortcuts, always-on-top windows, window reading and the updater are desktop-only."),
    h(
      "div",
      { class: "panes" },
      h("div", { class: "pane" }, sessions),
      h("div", { class: "pane wide" }, chat),
      staging(profile),
      hostFiles(profile),
      preview(),
      h("div", { class: "pane" }, cron),
    ),
  );
}
