// `/app`: the desktop workspace in the browser (spec: web-dashboard,
// Desktop workspace in the browser; Webapp file staging and terminals),
// served when `webapp.enabled = true`. It composes the shared pages
// (sessions, chat, scheduled jobs) with file staging: every file selected,
// pasted or dropped is staged under the addressed profile (16 MiB each,
// `POST stage`) before it enters the attachment flow. Native-only features
// (global shortcuts, always-on-top windows, window reading, the updater)
// are absent here.

import { api, ApiError, profilePath } from "../lib/api";
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
      h("div", { class: "pane" }, cron),
    ),
  );
}
