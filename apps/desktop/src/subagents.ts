// The live subagents frame (spec: desktop-app, Live subagents frame):
// while the chat's session has running children, a frame above its
// composer shows their count and, for up to 3, task, elapsed time and
// latest activity, expandable to the full roster; a selected child has
// Steer (queued for its next checkpoint) and Stop. Each chat has its own.

import type { SubagentList, SubagentSummary } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";

/** How often the frame refreshes while children run. */
const POLL_MS = 1_000;

const elapsed = (ms: number | null | undefined) => (ms ? `${Math.round(ms / 1000)} s` : "");

export class SubagentsFrame {
  readonly el = h("section", { class: "subagents-frame", "aria-label": "Subagents", hidden: true });
  private sessionId: string | null = null;
  private all = false;
  private selected: string | null = null;
  private timer: number | undefined;
  private readonly detail = h("div", { class: "subagent-detail" });

  constructor(private readonly client: DesktopClient) {
    client.on("session.event", (envelope) => {
      const type = (envelope.data as { type?: string }).type ?? "";
      if (envelope.session_id === this.sessionId && type.startsWith("subagent.")) void this.refresh();
    });
  }

  follow(sessionId: string | null): void {
    this.sessionId = sessionId;
    this.selected = null;
    this.detail.replaceChildren();
    window.clearInterval(this.timer);
    this.timer = sessionId ? window.setInterval(() => void this.refresh(), POLL_MS) : undefined;
    void this.refresh();
  }

  private async refresh(): Promise<void> {
    const sessionId = this.sessionId;
    if (!sessionId) {
      this.el.hidden = true;
      return;
    }
    const listed = await this.client.call<SubagentList>("subagent.list", { session_id: sessionId }).catch((): SubagentList => ({ subagents: [] }));
    if (sessionId !== this.sessionId) return;
    const running = listed.subagents.filter((s) => s.status === "running" || s.status === "queued");
    this.el.hidden = running.length === 0 && this.selected === null;
    const shown = this.all ? running : running.slice(0, 3);
    const more = h("button", { type: "button", name: "all-subagents" }, this.all ? "Show fewer" : `Show all ${running.length}`);
    more.addEventListener("click", () => {
      this.all = !this.all;
      void this.refresh();
    });
    this.el.replaceChildren(
      h("strong", { class: "subagent-count" }, `${running.length} subagent${running.length === 1 ? "" : "s"} running`),
      h("ul", {}, ...shown.map((s) => this.row(s))),
      ...(running.length > 3 ? [more] : []),
      this.detail,
    );
  }

  private row(child: SubagentSummary): HTMLElement {
    const select = h("button", { type: "button", name: "subagent", "data-subagent": child.subagent_id }, child.title ?? child.subagent_id);
    select.addEventListener("click", () => this.select(child));
    return h("li", { "data-subagent": child.subagent_id }, select, ` ${elapsed(child.elapsed_ms)} `, h("span", { class: "activity" }, child.activity ?? ""));
  }

  private select(child: SubagentSummary): void {
    this.selected = child.subagent_id;
    const text = h("input", { name: "steer-text", placeholder: "Guidance for this subagent", "aria-label": "Guidance" });
    const steer = h("button", { type: "button", name: "steer" }, "Steer");
    const stop = h("button", { type: "button", name: "stop-subagent" }, "Stop");
    const result = h("p", { class: "steer-result", role: "status" });
    steer.addEventListener("click", async () => {
      try {
        await this.client.call("subagent.steer", { session_id: this.sessionId, subagent_id: child.subagent_id, text: text.value });
        // Delivered at the child's next checkpoint, not read yet.
        result.textContent = "guidance queued for next checkpoint";
        text.value = "";
      } catch (err) {
        result.replaceChildren(notice((err as Error).message, "warn"));
      }
    });
    stop.addEventListener("click", async () => {
      try {
        await this.client.call("subagent.stop", { session_id: this.sessionId, subagent_id: child.subagent_id });
        result.textContent = "stop requested";
      } catch (err) {
        result.replaceChildren(notice((err as Error).message, "warn"));
      }
    });
    this.detail.replaceChildren(h("h4", {}, child.title ?? child.subagent_id), h("p", { class: "activity" }, child.activity ?? ""), text, steer, stop, result);
  }
}
