// The Artifacts view and the preview suggestions (spec: desktop-app,
// Artifacts gallery and suggestions): the images, files and links the
// profile's sessions produced (`artifacts.list`), searchable, each naming
// its session with a jump back, preview, download, open in browser and
// copy. Above the composer, files written in the current session are
// offered for preview; a dismissed suggestion stays dismissed across
// reloads until a later call writes the file again, and the file stays.

import type { Artifact, ArtifactList, Envelope, FsFile } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { type Opened, shell } from "./shell";

/** What the panes do with an artifact. */
export interface ArtifactActions {
  openSession(sessionId: string): void;
  preview(target: { url: string } | { sessionId: string; path: string }): void;
}

async function download(client: DesktopClient, a: Artifact): Promise<void> {
  const file = await client.call<FsFile>("fs.read", { session_id: a.session_id, path: a.value });
  const link = h("a", { href: `data:${file.mime ?? "application/octet-stream"};base64,${file.data}`, download: a.value.split("/").pop() ?? "download" });
  link.click();
}

export class ArtifactsView {
  readonly el = h("section", { class: "artifacts", "aria-label": "Artifacts" });
  private readonly search = h("input", { type: "search", name: "artifact-search", placeholder: "Search artifacts", "aria-label": "Search artifacts" });
  private readonly list = h("ul", { class: "artifact-list" });
  private readonly more = h("button", { type: "button", name: "more-artifacts", hidden: true }, "Load more");
  private cursor: string | null = null;

  constructor(
    private readonly client: DesktopClient,
    private readonly actions: ArtifactActions,
  ) {
    this.search.addEventListener("input", () => void this.load());
    this.more.addEventListener("click", () => void this.load(this.cursor));
    this.el.append(h("h3", {}, "Artifacts"), this.search, this.list, this.more);
  }

  /** Lists from the start (`cursor` null) or the next page. */
  async load(cursor: string | null = null): Promise<void> {
    const page = await this.client
      .call<ArtifactList>("artifacts.list", { profile: this.client.profile, query: this.search.value || null, cursor })
      .catch((err: unknown) => ({ artifacts: [], next_cursor: null, error: (err as Error).message }));
    const rows = page.artifacts.map((a) => this.row(a));
    if (cursor) this.list.append(...rows);
    else this.list.replaceChildren(...rows);
    this.cursor = page.next_cursor ?? null;
    this.more.hidden = this.cursor === null;
  }

  private row(a: Artifact): HTMLElement {
    const jump = h("button", { type: "button", name: "jump", "data-session": a.session_id }, a.session_title || a.session_id);
    jump.addEventListener("click", () => this.actions.openSession(a.session_id));
    const action = (name: string, label: string, run: () => Promise<void> | void) => {
      const button = h("button", { type: "button", name }, label);
      button.addEventListener("click", () => void Promise.resolve(run()).catch((err: unknown) => button.after(notice((err as Error).message, "warn"))));
      return button;
    };
    const link = a.kind === "link";
    return h(
      "li",
      { class: `artifact ${a.kind}`, "data-kind": a.kind, "data-value": a.value },
      h("span", { class: "value" }, a.value),
      " · ",
      jump,
      action("preview", "Preview", () => this.actions.preview(link ? { url: a.value } : { sessionId: a.session_id, path: a.value })),
      link ? null : action("download", "Download", () => download(this.client, a)),
      action("open-browser", "Open in browser", async () => {
        const opened = await shell<Opened>("open_external", { url: a.value });
        if (!opened.ok) throw new Error(`Not opened: ${opened.reason ?? "refused"}`);
      }),
      action("copy", "Copy", () => navigator.clipboard.writeText(a.value)),
    );
  }
}

const DISMISSED = "prometheus.suggestions.dismissed";

/** Dismissed suggestions: the write call each path was dismissed at. */
function dismissed(): Record<string, string> {
  try {
    return JSON.parse(localStorage.getItem(DISMISSED) ?? "{}") as Record<string, string>;
  } catch {
    return {};
  }
}

export class Suggestions {
  readonly el = h("div", { class: "suggestions", "aria-label": "Preview suggestions" });
  private sessionId: string | null = null;

  constructor(
    private readonly client: DesktopClient,
    private readonly actions: ArtifactActions,
  ) {
    client.on("turn.finished", (envelope: Envelope) => {
      if (envelope.session_id === this.sessionId) void this.follow(this.sessionId);
    });
  }

  /** Offers the files the session wrote (none for a draft). */
  async follow(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    if (!sessionId) {
      this.el.replaceChildren();
      return;
    }
    const page = await this.client.call<ArtifactList>("artifacts.list", { profile: this.client.profile, session_id: sessionId }).catch(() => ({ artifacts: [] as Artifact[] }));
    // The latest write of each path; artifacts come newest first.
    const latest = new Map<string, Artifact>();
    for (const a of page.artifacts) if (a.kind !== "link" && a.call_id && !latest.has(a.value)) latest.set(a.value, a);
    const gone = dismissed();
    const shown = [...latest.values()].filter((a) => gone[`${sessionId}:${a.value}`] !== a.call_id);
    this.el.replaceChildren(...shown.map((a) => this.suggestion(sessionId, a)));
  }

  private suggestion(sessionId: string, a: Artifact): HTMLElement {
    const open = h("button", { type: "button", name: "preview-suggestion", "data-path": a.value }, `Preview ${a.value}`);
    open.addEventListener("click", () => this.actions.preview({ sessionId, path: a.value }));
    const dismiss = h("button", { type: "button", name: "dismiss-suggestion", "aria-label": `Dismiss ${a.value}` }, "×");
    const row = h("span", { class: "suggestion", "data-path": a.value }, open, dismiss);
    dismiss.addEventListener("click", () => {
      const all = dismissed();
      all[`${sessionId}:${a.value}`] = a.call_id ?? "";
      try {
        localStorage.setItem(DISMISSED, JSON.stringify(all));
      } catch {
        // Dismissed for this window only.
      }
      row.remove();
    });
    return row;
  }
}
