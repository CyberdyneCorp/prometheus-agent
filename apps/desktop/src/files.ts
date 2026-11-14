// The file browser pane (spec: desktop-app, File browser pane): the
// session's workspace as a lazily loaded tree listed through the
// session's own terminal backend (`fs.list`), entries ignored by
// `.gitignore` hidden unless shown, text files up to 2 MiB and common
// images previewed inline (`fs.read`), anything else opened only through
// the guarded file opener. Expanded directories are listed again after
// each tool call that writes in the workspace.

import type { Envelope, FsEntry, FsFile, FsListing } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { isWebapp, type Opened, shell } from "./shell";
import { downloadHostFile } from "./web-shell";

/** The most a preview shows. */
export const PREVIEW_BYTES = 2 * 1024 * 1024;

/** Tools that may write in the workspace. */
const WRITERS = new Set(["write_file", "patch", "terminal", "execute_code"]);

const mib = (bytes: number) => `${(bytes / 1024 / 1024).toFixed(1)} MiB`;

function decode(file: FsFile): string {
  const bytes = Uint8Array.from(atob(file.data), (c) => c.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}

export class FileBrowser {
  readonly el = h("section", { class: "files", "aria-label": "Files" });
  private readonly tree = h("ul", { class: "file-tree", role: "tree" });
  private readonly preview = h("div", { class: "file-preview", "aria-label": "File preview" });
  private readonly ignored = h("input", { type: "checkbox", name: "show-ignored" });
  private readonly expanded = new Set<string>();
  private sessionId: string | null = null;

  constructor(private readonly client: DesktopClient) {
    this.ignored.addEventListener("change", () => void this.refresh());
    this.el.append(h("h3", {}, "Files"), h("label", {}, this.ignored, " Show ignored files"), this.tree, this.preview);
    client.on("session.event", (envelope) => this.observe(envelope));
  }

  /** Shows `sessionId`'s workspace (nothing for a draft). */
  async follow(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    this.expanded.clear();
    this.preview.replaceChildren();
    await this.refresh();
  }

  /** Expands the tree down to `path` and previews it. */
  async reveal(path: string): Promise<void> {
    const parts = path.replace(/^\.\//, "").split("/");
    for (let i = 1; i < parts.length; i++) this.expanded.add(parts.slice(0, i).join("/"));
    await this.refresh();
    const row = this.tree.querySelector<HTMLElement>(`[data-path="${CSS.escape(parts.join("/"))}"]`);
    row?.setAttribute("aria-selected", "true");
    row?.scrollIntoView({ block: "nearest" });
    await this.open(parts.join("/"));
  }

  private observe(envelope: Envelope): void {
    if (envelope.session_id !== this.sessionId || envelope.event !== "session.event") return;
    const data = envelope.data as { type?: string; payload?: { tool?: string; ok?: boolean } };
    if (data.type === "tool.result" && data.payload?.ok && WRITERS.has(String(data.payload.tool))) void this.refresh();
  }

  private async refresh(): Promise<void> {
    if (!this.sessionId) {
      this.tree.replaceChildren();
      return;
    }
    this.tree.replaceChildren(...(await this.level(".")));
  }

  private async level(path: string): Promise<HTMLElement[]> {
    try {
      const listing = await this.client.call<FsListing>("fs.list", { session_id: this.sessionId, path, show_ignored: this.ignored.checked });
      return Promise.all(listing.entries.map((entry) => this.row(entry)));
    } catch (err) {
      return [h("li", { class: "notice warn" }, (err as Error).message)];
    }
  }

  private async row(entry: FsEntry): Promise<HTMLElement> {
    const button = h("button", { type: "button", class: entry.dir ? "dir" : "file", "data-path": entry.path }, entry.dir ? `${entry.name}/` : entry.name);
    const li = h("li", { role: "treeitem", "data-path": entry.path, "data-ignored": entry.ignored ? "true" : null }, button);
    if (!entry.dir) {
      button.addEventListener("click", () => void this.open(entry.path));
      return li;
    }
    const children = h("ul", { role: "group" });
    li.append(children);
    button.setAttribute("aria-expanded", String(this.expanded.has(entry.path)));
    if (this.expanded.has(entry.path)) children.append(...(await this.level(entry.path)));
    button.addEventListener("click", async () => {
      const open = !this.expanded.has(entry.path);
      if (open) this.expanded.add(entry.path);
      else this.expanded.delete(entry.path);
      button.setAttribute("aria-expanded", String(open));
      children.replaceChildren(...(open ? await this.level(entry.path) : []));
    });
    return li;
  }

  /** Previews `path`: text and images inline, the first 2 MiB of a
   *  larger file with a notice, anything else through the opener. */
  async open(path: string): Promise<void> {
    if (!this.sessionId) return;
    let file: FsFile;
    try {
      file = await this.client.call<FsFile>("fs.read", { session_id: this.sessionId, path, max_bytes: PREVIEW_BYTES });
    } catch (err) {
      this.preview.replaceChildren(h("h4", {}, path), notice((err as Error).message, "warn"));
      return;
    }
    // The webapp downloads it through the session's file-access rules.
    const external = isWebapp() ? h("button", { type: "button", name: "download" }, "Download") : h("button", { type: "button", name: "open-externally" }, "Open externally");
    external.addEventListener("click", () => void (isWebapp() ? this.download(file) : this.external(file)));
    const body =
      file.kind === "image" && file.mime
        ? h("img", { src: `data:${file.mime};base64,${file.data}`, alt: path })
        : file.kind === "text"
          ? h("pre", { class: "file-text" }, decode(file))
          : notice("No preview for this kind of file.", "info");
    const cut = file.truncated ? notice(`Showing the first ${mib(PREVIEW_BYTES)} of ${mib(file.size)}.`, "info") : null;
    this.preview.replaceChildren(h("h4", { "data-path": path }, path), ...(cut ? [cut] : []), body, external);
  }

  private async download(file: FsFile): Promise<void> {
    if (!this.sessionId) return;
    try {
      await downloadHostFile(this.client.profile, this.sessionId, file.path);
    } catch (err) {
      this.preview.append(notice(`Not downloaded: ${(err as Error).message}`, "warn"));
    }
  }

  private async external(file: FsFile): Promise<void> {
    if (!file.absolute) {
      this.preview.append(notice("This file is not on this machine; it cannot be opened here.", "warn"));
      return;
    }
    const opened = await shell<Opened>("open_file", { path: file.absolute, workspace: file.workspace ?? null });
    if (!opened.ok) this.preview.append(notice(`Not opened: ${opened.reason ?? "refused"}`, "warn"));
  }
}
