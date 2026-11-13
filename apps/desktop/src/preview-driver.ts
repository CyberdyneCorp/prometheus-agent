// The app side of the preview tools (spec: desktop-app, Preview pane tool;
// Driving the preview page; Annotating the preview page): asks the agent
// in the preview frame (preview-agent.ts) to read, drive or mark its page.
// The page may be of any origin, so everything goes through messages.
//
// Each document the frame loads announces itself (`preview.ready`); the
// driver counts them. Refs come from one `elements` answer and belong to
// that document: a ref used after the frame loaded another one is stale,
// whatever the new page numbers its own elements. An action that makes
// the page navigate may never be answered by the page it left, so the
// driver answers it from the next `preview.ready`.

import type { PreviewRail } from "./preview";

type Args = Record<string, unknown>;
type Result = Record<string, unknown>;

/** How long a page has to answer one operation. */
const ANSWER_MS = 10_000;
/** How long a navigation an action caused has to show its new page. */
const NAVIGATION_MS = 5_000;

export class PreviewDriver {
  private next = 1;
  private readonly pending = new Map<number, (result: Result) => void>();
  /** Documents the open tab has loaded. */
  private documents = 0;
  /** The document the last `elements` refs belong to (-1: none yet). */
  private refsOf = -1;
  private address = "";
  private title = "";
  private readonly loaded: (() => void)[] = [];

  constructor(private readonly rail: PreviewRail) {
    rail.onOpened = () => {
      this.documents = 0;
      this.refsOf = -1;
    };
    window.addEventListener("message", (event) => {
      if (!rail.current || event.source !== rail.current.contentWindow) return;
      const data = event.data as { prometheus?: string; id?: number; result?: Result; url?: string; title?: string } | null;
      if (data?.prometheus === "preview.result") {
        this.pending.get(data.id ?? 0)?.(data.result ?? {});
        this.pending.delete(data.id ?? 0);
      } else if (data?.prometheus === "preview.ready") {
        this.documents += 1;
        this.address = data.url ?? "";
        this.title = data.title ?? "";
        for (const resume of this.loaded.splice(0)) resume();
      }
    });
  }

  /** `desktop_preview read`: the visible text of a browser tab, or what a
   *  file tab shows. */
  async read(args: Args): Promise<Result> {
    const page = this.rail.page;
    if (!page) return noTab();
    if (page.kind === "file") return { kind: "file", path: page.address, label: page.label };
    return this.ask("read", args);
  }

  /** `drive_preview`. */
  async drive(args: Args): Promise<Result> {
    const page = this.rail.page;
    if (!page) return noTab();
    if (page.kind !== "web") return { success: false, error: "drive_preview acts on web pages; this tab shows a workspace file" };
    const stale = this.stale(args);
    if (stale) return stale;
    if (args.action === "elements") {
      const result = await this.ask("elements", args);
      this.refsOf = this.documents;
      return result;
    }
    const before = this.documents;
    const result = await Promise.race([this.ask("act", args), this.navigation(before)]);
    if (result.navigating !== true && result.navigated !== true && this.documents === before) return result;
    if (this.documents === before) await Promise.race([this.navigation(before), delay(NAVIGATION_MS)]);
    return { success: true, action: args.action, navigated: this.documents !== before, url: this.address, title: this.title, note: "the page navigated: refs from before are stale; call elements again" };
  }

  /** `annotate_preview`. */
  async annotate(args: Args): Promise<Result> {
    const page = this.rail.page;
    if (!page || page.kind !== "web") return page ? { success: false, error: "annotate_preview marks web pages" } : noTab();
    return this.stale(args) ?? this.ask("mark", args);
  }

  /** The stale-ref answer when `args` names a ref of an earlier document. */
  private stale(args: Args): Result | null {
    if (typeof args.ref !== "string" || !args.ref) return null;
    if (this.refsOf === this.documents) return null;
    const why = this.refsOf < 0 ? "no elements call has listed refs for this page" : "the page navigated since the elements call that listed it";
    return { success: false, stale: true, error: `ref ${args.ref} is stale: ${why}; call drive_preview elements for new refs` };
  }

  private navigation(before: number): Promise<Result> {
    return new Promise((resolve) => {
      const check = () => (this.documents > before ? resolve({ navigated: true }) : this.loaded.push(check));
      check();
    });
  }

  private async ask(op: string, args: Args): Promise<Result> {
    // A tab still loading its first document has no agent to ask yet.
    if (this.documents === 0) await Promise.race([this.navigation(0), delay(NAVIGATION_MS)]);
    const frame = this.rail.current?.contentWindow;
    if (!frame) return noTab();
    const id = this.next++;
    return new Promise((resolve) => {
      const timer = window.setTimeout(() => {
        this.pending.delete(id);
        resolve({ success: false, error: "the preview page did not answer; it may still be loading" });
      }, ANSWER_MS);
      this.pending.set(id, (result) => {
        window.clearTimeout(timer);
        resolve(result);
      });
      frame.postMessage({ prometheus: "preview", id, op, args }, "*");
    });
  }
}

function noTab(): Result {
  return { success: false, error: "no preview tab is open; open one with desktop_preview" };
}

function delay(ms: number): Promise<Result> {
  return new Promise((resolve) => window.setTimeout(() => resolve({}), ms));
}
