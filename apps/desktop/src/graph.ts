// The memory graph (spec: desktop-app, Memory graph): the profile's
// learned skills and memories laid out over time, with a playback
// scrubber and the `all`, `used` and `learned` filters. Editing or
// deleting a node goes through the same `manage.change` memory and skill
// changes as every surface (deleting a skill archives it); live sessions
// keep their snapshot and see the change from their next session. Share
// exports a layout code of node ids and positions only, never a
// memory's or a skill's text.

import type { JourneyList, JourneyNode } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";

const WIDTH = 760;
const LANES: Record<string, number> = { skill: 70, memory: 190 };
const LEARNED = ["user", "agent", "background_review"];

export type Filter = "all" | "used" | "learned";

export interface Placed {
  node: JourneyNode;
  x: number;
  y: number;
}

/** A small stable number of `text` (a node's offset in its lane). */
function hash(text: string): number {
  let n = 0;
  for (const c of text) n = (n * 31 + c.charCodeAt(0)) >>> 0;
  return n;
}

/** The nodes placed over time: x by when they were learned, y by kind. */
export function layout(nodes: JourneyNode[]): Placed[] {
  const times = nodes.map((n) => Date.parse(n.created_at));
  const first = Math.min(...times);
  const span = Math.max(1, Math.max(...times) - first);
  return nodes.map((node, i) => ({
    node,
    x: Math.round(20 + ((times[i] ?? first) - first) / span * (WIDTH - 40)),
    y: (LANES[node.kind] ?? 130) + (hash(node.id) % 60) - 30,
  }));
}

export function passes(node: JourneyNode, filter: Filter): boolean {
  if (filter === "used") return (node.uses ?? 0) > 0;
  if (filter === "learned") return LEARNED.includes(node.origin);
  return true;
}

/** The share code: ids and positions only. */
export function shareCode(placed: Placed[]): string {
  const layoutOnly = { v: 1, nodes: placed.map((p) => ({ id: p.node.id, x: p.x, y: p.y })) };
  return `pmg1.${btoa(JSON.stringify(layoutOnly))}`;
}

export class MemoryGraph {
  readonly el = h("section", { class: "memory-graph", role: "dialog", "aria-label": "Memory graph", hidden: true });
  private readonly filter = h("select", { name: "graph-filter", "aria-label": "Filter" }, ...["all", "used", "learned"].map((f) => h("option", { value: f }, f)));
  private readonly scrubber = h("input", { type: "range", name: "playback", min: 0, value: 0, "aria-label": "Playback" });
  private readonly svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  private readonly detail = h("div", { class: "graph-node" });
  private readonly share = h("input", { name: "share-code", readonly: true, "aria-label": "Share code", hidden: true });
  private placed: Placed[] = [];

  constructor(private readonly client: DesktopClient) {
    this.svg.setAttribute("viewBox", `0 0 ${WIDTH} 260`);
    this.svg.setAttribute("class", "graph");
    this.filter.addEventListener("change", () => this.draw());
    this.scrubber.addEventListener("input", () => this.draw());
    const share = h("button", { type: "button", name: "share-graph" }, "Share");
    share.addEventListener("click", () => {
      this.share.hidden = false;
      this.share.value = shareCode(this.visible());
    });
    const close = h("button", { type: "button", name: "close-graph" }, "Close");
    close.addEventListener("click", () => {
      this.el.hidden = true;
    });
    this.el.append(h("header", {}, h("h2", {}, "Memory graph"), this.filter, this.scrubber, share, close), this.svg, this.share, this.detail);
  }

  async open(): Promise<void> {
    this.el.hidden = false;
    delete this.el.dataset.ready;
    this.detail.replaceChildren();
    this.share.hidden = true;
    const listed = await this.client.call<JourneyList>("journey.list", { profile: this.client.profile });
    this.placed = layout(listed.nodes);
    this.scrubber.max = String(this.placed.length);
    this.scrubber.value = String(this.placed.length);
    this.draw();
    this.el.dataset.ready = "true";
  }

  /** The nodes the scrubber and the filter leave. */
  private visible(): Placed[] {
    const until = Number(this.scrubber.value);
    return this.placed.slice(0, until).filter((p) => passes(p.node, this.filter.value as Filter));
  }

  private draw(): void {
    const ns = "http://www.w3.org/2000/svg";
    this.svg.replaceChildren(
      ...this.visible().map((p) => {
        const dot = document.createElementNS(ns, "circle");
        for (const [k, v] of Object.entries({ cx: p.x, cy: p.y, r: 7, class: `node ${p.node.kind}`, "data-node": p.node.id, tabindex: 0 })) dot.setAttribute(k, String(v));
        const title = document.createElementNS(ns, "title");
        title.textContent = p.node.title;
        dot.append(title);
        dot.addEventListener("click", () => void this.select(p.node));
        return dot;
      }),
    );
  }

  private async select(node: JourneyNode): Promise<void> {
    const applies = h("p", { class: "muted" }, "Changes apply to new sessions; open chats keep what they started with.");
    const remove = h("button", { type: "button", name: "delete-node" }, node.kind === "skill" ? "Archive skill" : "Delete memory");
    remove.addEventListener("click", () => void this.remove(node));
    const edit = node.kind === "memory" ? await this.editor(node) : null;
    this.detail.replaceChildren(h("h3", {}, node.title), h("p", {}, `${node.kind} · ${node.origin} · ${node.created_at} · used ${node.uses ?? 0}×`), edit ?? "", remove, applies);
    this.detail.dataset.node = node.id;
  }

  private async editor(node: JourneyNode): Promise<HTMLElement> {
    const listed = await this.client.call<{ entries: { id: string; content: string }[] }>("manage.catalog", { profile: this.client.profile, catalog: "memory" });
    const text = h("textarea", { name: "node-text", rows: 3, "aria-label": "Memory text" });
    text.value = listed.entries.find((e) => e.id === node.id)?.content ?? node.title;
    const save = h("button", { type: "button", name: "save-node" }, "Save");
    save.addEventListener("click", () => void this.change({ change: "memory", body: { replace: { id: node.id, text: text.value } } }));
    return h("div", {}, text, save);
  }

  private async remove(node: JourneyNode): Promise<void> {
    const skill = node.kind === "skill";
    await this.change(skill ? { change: "skill_archive", target: node.id.replace(/^skill:/, "") } : { change: "memory", body: { remove: node.id } });
  }

  private async change(params: Record<string, unknown>): Promise<void> {
    try {
      await this.client.call("manage.change", { profile: this.client.profile, ...params });
      await this.open();
    } catch (err) {
      this.detail.append(notice((err as Error).message, "error"));
    }
  }
}
