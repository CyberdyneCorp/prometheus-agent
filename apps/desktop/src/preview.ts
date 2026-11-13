// The preview rail and in-app browser (spec: desktop-app, Preview rail and
// in-app browser): web pages, workspace files and tool outputs beside the
// chat in a sandboxed frame of its own origin, which has no native
// command channel (the shell is granted to the app's own origin only),
// no access to the app's storage and no client protocol ticket. Popups
// and downloads are not allowed to it, and only `http`, `https` and
// workspace files load; a session's file is read through the session's
// connection (`fs.read`). Hide keeps the page mounted with its state and
// inert to shortcuts; Close releases it.

import type { FsFile } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import { Annotator } from "./annotate";
import type { DesktopClient } from "./client";

/** What the rail shows: a web page, or a file of a session. */
export type PreviewTarget = { url: string } | { sessionId: string; path: string };

/** `null` when the rail may load `url`, else why not. */
export function refusal(url: string): string | null {
  try {
    const parsed = new URL(url);
    return parsed.protocol === "http:" || parsed.protocol === "https:" ? null : `${parsed.protocol} links do not open in the preview`;
  } catch {
    return "not a URL";
  }
}

export class PreviewRail {
  readonly el = h("section", { class: "preview", "aria-label": "Preview", hidden: true });
  readonly annotate: Annotator;
  private readonly address = h("input", { name: "preview-url", placeholder: "https://", "aria-label": "Address" });
  private readonly status = h("div", { class: "preview-status" });
  private readonly body = h("div", { class: "preview-body" });
  private frame: HTMLIFrameElement | null = null;
  /** What the open tab shows, and its label. */
  page: { kind: "web" | "file"; address: string; label: string } | null = null;
  /** A new tab was opened (the driver starts over). */
  onOpened: () => void = () => {};

  constructor(
    private readonly client: DesktopClient,
    composerAttach: Annotator["attach"],
  ) {
    const go = h("button", { type: "button", name: "preview-go" }, "Go");
    go.addEventListener("click", () => void this.open({ url: this.address.value }));
    const hide = h("button", { type: "button", name: "preview-hide" }, "Hide");
    hide.addEventListener("click", () => this.hide());
    const close = h("button", { type: "button", name: "preview-close" }, "Close");
    close.addEventListener("click", () => this.close());
    this.annotate = new Annotator(() => this.frame, composerAttach);
    this.el.append(h("div", { class: "preview-bar" }, this.address, go, this.annotate.toggle, hide, close), this.status, this.body, this.annotate.el);
  }

  /** Whether a page is loaded (hidden or not). */
  get mounted(): boolean {
    return this.frame !== null;
  }

  /** The open tab's frame. */
  get current(): HTMLIFrameElement | null {
    return this.frame;
  }

  /** Opens `target` as the rail's tab (named `label`); whether it opened
   *  (else the rail's status says why). */
  async open(target: PreviewTarget, label?: string): Promise<boolean> {
    this.status.replaceChildren();
    this.show();
    const loaded = "url" in target ? this.web(target.url) : await this.file(target.sessionId, target.path);
    if (!loaded) return false;
    this.close();
    this.el.hidden = false;
    this.frame = loaded;
    const address = "url" in target ? target.url : target.path;
    this.page = { kind: "url" in target ? "web" : "file", address, label: label ?? address };
    this.onOpened();
    this.frame.addEventListener("load", () => {
      this.frame?.setAttribute("data-loads", String(Number(this.frame.dataset.loads ?? "0") + 1));
    });
    this.body.append(this.frame);
    return true;
  }

  /** Why the last open failed. */
  get problem(): string {
    return this.status.textContent ?? "";
  }

  private web(url: string): HTMLIFrameElement | null {
    const why = refusal(url);
    if (why) {
      this.status.replaceChildren(notice(why, "warn"));
      return null;
    }
    this.address.value = url;
    // Its own origin; no popups, no downloads, no top navigation.
    return h("iframe", { src: url, sandbox: "allow-scripts allow-forms allow-same-origin", referrerpolicy: "no-referrer", title: "Preview" });
  }

  private async file(sessionId: string, path: string): Promise<HTMLIFrameElement | null> {
    try {
      const file = await this.client.call<FsFile>("fs.read", { session_id: sessionId, path });
      const type = file.kind === "image" ? (file.mime ?? "image/png") : /\.html?$/i.test(path) ? "text/html" : "text/plain";
      const blob = new Blob([Uint8Array.from(atob(file.data), (c) => c.charCodeAt(0))], { type });
      this.address.value = path;
      // An opaque origin: the file's scripts reach nothing of the app.
      return h("iframe", { src: URL.createObjectURL(blob), sandbox: "allow-scripts allow-forms", title: `Preview of ${path}` });
    } catch (err) {
      this.status.replaceChildren(notice((err as Error).message, "warn"));
      return null;
    }
  }

  /** Hide: the page stays mounted with its state, inert. */
  hide(): void {
    this.el.hidden = true;
    this.el.inert = true;
  }

  show(): void {
    this.el.hidden = false;
    this.el.inert = false;
  }

  /** Close: the page is released. */
  close(): void {
    this.annotate.clear();
    this.frame?.remove();
    this.frame = null;
    this.page = null;
  }
}
