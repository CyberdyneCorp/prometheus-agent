// The queued prompts above the composer (spec: desktop-app, Composer
// history and queue editing): the daemon holds the session's queue, so
// every attached window shows the same list (`queue.changed`) and it
// survives restarts. A turn whose outcome a daemon restart lost has one
// action, Discard, which unblocks the queue without sending it again.

import type { QueueEntry, QueueState } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";

export class QueueView {
  readonly el = h("section", { class: "queue", "aria-label": "Queued prompts", hidden: true });
  private sessionId: string | null = null;
  state: QueueState | null = null;

  constructor(private readonly client: DesktopClient) {
    client.on("queue.changed", (envelope) => {
      const state = envelope.data as QueueState;
      if (state.session_id === this.sessionId) this.show(state);
    });
  }

  /** Follows `sessionId`'s queue (none for a draft). */
  async follow(sessionId: string | null): Promise<void> {
    this.sessionId = sessionId;
    this.show(null);
    if (!sessionId) return;
    try {
      this.show(await this.client.call<QueueState>("queue.list", { session_id: sessionId }));
    } catch (err) {
      console.warn("queue unavailable:", err);
    }
  }

  /** Whether prompts wait in the queue. */
  get waiting(): boolean {
    return (this.state?.entries.length ?? 0) > 0;
  }

  async pause(): Promise<void> {
    if (this.sessionId) await this.call("queue.pause", { session_id: this.sessionId });
  }

  private async call(method: string, params: Record<string, unknown>): Promise<void> {
    try {
      this.show(await this.client.call<QueueState>(method, params));
    } catch (err) {
      this.el.append(notice((err as Error).message, "error"));
    }
  }

  private show(state: QueueState | null): void {
    this.state = state;
    const entries = state?.entries ?? [];
    this.el.hidden = entries.length === 0 && !state?.paused;
    const rows = entries.map((entry) => (entry.lost ? this.lost(entry) : this.queued(entry)));
    const paused = state?.paused ? this.paused() : null;
    this.el.replaceChildren(...(paused ? [paused] : []), h("ol", {}, ...rows));
  }

  private paused(): HTMLElement {
    const resume = h("button", { type: "button", name: "queue-resume" }, "Resume queue");
    resume.addEventListener("click", () => void this.call("queue.resume", { session_id: this.sessionId }));
    return h("p", { class: "queue-paused", role: "status" }, "Queue paused ", resume);
  }

  private lost(entry: QueueEntry): HTMLElement {
    const discard = h("button", { type: "button", name: "discard" }, "Discard");
    discard.addEventListener("click", () => void this.call("queue.delete", { session_id: this.sessionId, id: entry.id }));
    return h("li", { class: "queue-entry lost", "data-entry": entry.id }, h("strong", {}, "Turn lost during restart"), h("span", { class: "text" }, entry.text), discard);
  }

  private queued(entry: QueueEntry): HTMLElement {
    const text = h("span", { class: "text" }, entry.text);
    const edit = h("button", { type: "button", name: "edit" }, "Edit");
    const remove = h("button", { type: "button", name: "delete" }, "Delete");
    const row = h("li", { class: "queue-entry", "data-entry": entry.id }, text, edit, remove);
    remove.addEventListener("click", () => void this.call("queue.delete", { session_id: this.sessionId, id: entry.id }));
    edit.addEventListener("click", () => {
      const input = h("input", { name: "queue-text", value: entry.text, "aria-label": "Queued prompt" });
      const save = h("button", { type: "button", name: "save" }, "Save");
      save.addEventListener("click", () => void this.call("queue.edit", { session_id: this.sessionId, id: entry.id, text: input.value }));
      row.replaceChildren(input, save);
      input.focus();
    });
    return row;
  }
}
