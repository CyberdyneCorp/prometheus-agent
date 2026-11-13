// The desktop chat transcript (spec: desktop-app, Chat transcript
// rendering): the dashboard's live rendering plus a timeline rail with one
// marker per prompt once a session has 4 or more, paging (`Show earlier`),
// cron and delegation completions as collapsed disclosures whose body is
// the result only, tool rows that expand to the images the tool produced
// (read through the session's own connection, never from a local path),
// and following new output only while the view is at the bottom.

import type { Envelope, TranscriptMessage } from "../../../schema/rpc";
import { Transcript } from "../../../web/src/components/transcript";
import { h } from "../../../web/src/lib/dom";

/** Prompts rendered at first and added by each `Show earlier`. */
export const PAGE = 10;
/** The rail shows from this many prompts on. */
export const RAIL_FROM = 4;
/** Within this many pixels of the bottom, the view follows new output. */
const AT_BOTTOM = 8;

const IMAGE_PATH = /(?:\/|\.\/|~\/|[A-Za-z]:\\)?[\w@~.\\/-]*[\w-]\.(?:png|jpe?g|gif|webp|svg|bmp)\b/gi;

/** The image paths a tool's output names. */
export function imagePaths(output: string): string[] {
  return [...new Set(output.match(IMAGE_PATH) ?? [])];
}

/** A cron or delegation completion: its title and the result only. */
export function completion(text: string): { title: string; body: string } | null {
  const cron = /^\[cron run finished\] job (.+?) \(([^)]*)\)[^\n]*\n\n([\s\S]*)$/.exec(text);
  if (cron) return { title: `Scheduled job ${cron[1]} finished`, body: cron[3] ?? "" };
  if (text.startsWith("[delegation_completed ")) {
    const json = text.slice(text.indexOf("\n") + 1);
    try {
      const parsed = JSON.parse(json) as { label?: string; results?: { summary?: string; status?: string }[] };
      const body = (parsed.results ?? []).map((r) => r.summary ?? r.status ?? "").join("\n\n");
      return { title: parsed.label ? `${parsed.label}: subagents finished` : "Subagents finished", body };
    } catch {
      return { title: "Subagents finished", body: json };
    }
  }
  return null;
}

export class DesktopTranscript extends Transcript {
  readonly rail = h("nav", { class: "timeline", "aria-label": "Timeline" });
  readonly frame: HTMLElement;
  /** Reads an image of the session through its connection (a data URL). */
  fetchImage: (path: string) => Promise<string> = () => Promise.reject(new Error("no session"));
  /** Reveal in file tree, for a file a tool call named. */
  onReveal: ((path: string) => void) | null = null;
  /** The file each running tool call names. */
  private readonly callPaths = new Map<string, string>();
  /** The session's stored transcript, fresh (a jump re-reads it). */
  source: () => Promise<TranscriptMessage[]> = () => Promise.resolve([]);
  private messages: TranscriptMessage[] = [];
  /** Index in `messages` of the first rendered one. */
  private start = 0;
  /** The number the next rendered prompt gets. */
  private nextPrompt = 1;
  private prompts: string[] = [];
  private rendering = false;

  constructor() {
    super();
    this.frame = h("div", { class: "transcript-frame" }, this.rail, this.el);
  }

  override load(messages: TranscriptMessage[]): void {
    this.messages = messages.filter((m) => m.text && (m.role === "user" || m.role === "assistant"));
    this.prompts = this.messages.filter(isPrompt).map((m) => m.text ?? "");
    const starts = this.promptIndices();
    this.start = starts.length > PAGE ? (starts[starts.length - PAGE] ?? 0) : 0;
    this.render();
    this.el.scrollTop = this.el.scrollHeight;
  }

  override message(role: string, text: string): HTMLElement {
    const done = role === "user" ? completion(text) : null;
    const el = done
      ? h("details", { class: "completion" }, h("summary", {}, done.title), h("div", { class: "text" }, done.body))
      : super.message(role, text);
    if (done) this.add(el);
    if (role === "user" && !done) {
      el.dataset.prompt = String(this.nextPrompt++);
      if (!this.rendering) {
        this.prompts.push(text);
        this.messages.push({ id: `live-${this.prompts.length}`, role, text });
        this.renderRail();
      }
    }
    return el;
  }

  /** Follows new output only when the view was at the bottom. */
  protected override add(el: HTMLElement): void {
    const follow = this.rendering || this.distanceFromBottom() <= AT_BOTTOM;
    this.el.append(el);
    if (follow) this.el.scrollTop = this.el.scrollHeight;
  }

  /** How far the view is from the bottom, in pixels. */
  distanceFromBottom(): number {
    return Math.max(0, this.el.scrollHeight - this.el.clientHeight - this.el.scrollTop);
  }

  /** Puts the view back at a saved distance from the bottom. */
  restore(distance: number): void {
    this.el.scrollTop = Math.max(0, this.el.scrollHeight - this.el.clientHeight - distance);
  }

  /** Shows prompt `n` (1-based), rendering from it when it is not shown. */
  async jump(n: number): Promise<void> {
    if (!this.el.querySelector(`[data-prompt="${n}"]`)) {
      const fresh = await this.source();
      this.messages = fresh.filter((m) => m.text && (m.role === "user" || m.role === "assistant"));
      this.prompts = this.messages.filter(isPrompt).map((m) => m.text ?? "");
      this.start = this.promptIndices()[n - 1] ?? 0;
      this.render();
    }
    this.el.querySelector(`[data-prompt="${n}"]`)?.scrollIntoView({ block: "start" });
  }

  /** Renders the page of prompts before the first shown one. */
  showEarlier(): void {
    const starts = this.promptIndices().filter((i) => i < this.start);
    const first = this.el.querySelector<HTMLElement>("[data-prompt]")?.dataset.prompt;
    this.start = starts.length > PAGE ? (starts[starts.length - PAGE] ?? 0) : 0;
    this.render();
    // The prompt the user was reading stays in view.
    if (first) this.el.querySelector(`[data-prompt="${first}"]`)?.scrollIntoView({ block: "start" });
  }

  override apply(envelope: Envelope): void {
    const data = envelope.data as { type?: string; payload?: { call_id?: string; arguments?: unknown } };
    if (envelope.event === "session.event" && data.type === "tool.call") {
      const args = typeof data.payload?.arguments === "string" ? safeJson(data.payload.arguments) : data.payload?.arguments;
      const path = (args as { path?: unknown } | undefined)?.path;
      if (typeof path === "string") this.callPaths.set(String(data.payload?.call_id), path);
    }
    super.apply(envelope);
  }

  protected override toolResult(p: Record<string, unknown>): void {
    super.toolResult(p);
    const row = this.tools.get(String(p.call_id));
    const named = this.callPaths.get(String(p.call_id));
    if (row && named && this.onReveal) {
      const reveal = h("button", { type: "button", name: "reveal", "data-path": named }, "Reveal in file tree");
      const open = this.onReveal;
      reveal.addEventListener("click", () => open(named));
      row.append(reveal);
    }
    const paths = p.ok ? imagePaths(String(p.output ?? "")) : [];
    if (!row || paths.length === 0) return;
    const images = h("div", { class: "tool-images" });
    const more = h("details", { class: "tool-detail" }, h("summary", {}, `Images (${paths.length})`), images);
    more.addEventListener("toggle", () => {
      if (!more.open || images.childElementCount > 0) return;
      for (const path of paths) void this.image(path, images);
    });
    row.append(more);
  }

  private async image(path: string, into: HTMLElement): Promise<void> {
    try {
      into.append(h("img", { src: await this.fetchImage(path), alt: path, title: path, "data-path": path }));
    } catch (err) {
      into.append(h("p", { class: "notice warn" }, `${path}: ${(err as Error).message}`));
    }
  }

  private promptIndices(): number[] {
    return this.messages.flatMap((m, i) => (isPrompt(m) ? [i] : []));
  }

  private render(): void {
    this.rendering = true;
    this.el.replaceChildren();
    this.tools.clear();
    if (this.start > 0) {
      const earlier = h("button", { type: "button", name: "show-earlier", class: "show-earlier" }, "Show earlier");
      earlier.addEventListener("click", () => this.showEarlier());
      this.el.append(earlier);
    }
    this.nextPrompt = this.messages.slice(0, this.start).filter(isPrompt).length + 1;
    for (const m of this.messages.slice(this.start)) this.message(m.role, m.text ?? "");
    this.rendering = false;
    this.renderRail();
  }

  private renderRail(): void {
    if (this.prompts.length < RAIL_FROM) {
      this.rail.replaceChildren();
      this.rail.hidden = true;
      return;
    }
    this.rail.hidden = false;
    this.rail.replaceChildren(
      ...this.prompts.map((text, i) => {
        const marker = h("button", { type: "button", class: "marker", "data-marker": i + 1, title: text, "aria-label": `Prompt ${i + 1}: ${text}` });
        marker.addEventListener("click", () => void this.jump(i + 1));
        return marker;
      }),
    );
  }
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

function isPrompt(m: TranscriptMessage): boolean {
  return m.role === "user" && completion(m.text ?? "") === null;
}
