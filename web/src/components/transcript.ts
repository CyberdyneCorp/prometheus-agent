// The chat transcript (shared with the desktop): stored messages, then the
// live turn rendered from client protocol events: streamed answer text,
// tool calls with their results, subagent status, approval and clarify
// cards, and the turn's end.

import type { Envelope, TranscriptMessage, TurnFinished } from "../../../schema/rpc";
import { h } from "../lib/dom";

interface SessionEventData {
  type: string;
  turn_id?: string | null;
  payload: Record<string, unknown>;
}

export class Transcript {
  readonly el = h("div", { class: "transcript", role: "log", "aria-live": "polite" });
  private answer: HTMLElement | null = null;
  /** The running turn showed answer text. */
  private answered = false;
  protected tools = new Map<string, HTMLElement>();
  private subagents = new Map<string, HTMLElement>();
  private cards = new Map<string, HTMLElement>();

  load(messages: TranscriptMessage[]): void {
    this.el.replaceChildren();
    for (const m of messages) {
      if (m.text && (m.role === "user" || m.role === "assistant")) this.message(m.role, m.text);
    }
  }

  message(role: string, text: string): HTMLElement {
    const el = h("div", { class: `message ${role}`, "data-role": role }, h("span", { class: "who" }, role === "user" ? "You" : "Agent"), h("div", { class: "text" }, text));
    this.add(el);
    return el;
  }

  /** Appends a row of the live turn and follows the output. */
  protected add(el: HTMLElement): void {
    this.el.append(el);
    this.el.scrollTop = this.el.scrollHeight;
  }

  /** Applies one client protocol event of this session. */
  apply(envelope: Envelope): void {
    if (envelope.event === "session.event") this.sessionEvent(envelope.data as SessionEventData);
    else if (envelope.event === "turn.finished") this.finished(envelope.data as TurnFinished);
    else if (envelope.event === "request.resolved" || envelope.event === "request.cancel") {
      this.settle(String((envelope.data as { id?: string }).id ?? ""), envelope.event === "request.cancel");
    }
  }

  /** A request was answered (here or by another client) or withdrawn:
   *  its card stays as a record but takes no more answers. */
  private settle(id: string, withdrawn: boolean): void {
    const card = this.cards.get(id);
    if (!card) return;
    this.cards.delete(id);
    for (const control of Array.from(card.querySelectorAll<HTMLButtonElement | HTMLInputElement>("button, input"))) control.disabled = true;
    const status = card.querySelector<HTMLElement>(".card-status");
    if (status && !status.textContent) status.textContent = withdrawn ? "No longer waiting" : "Answered in another client";
  }

  card(id: string, el: HTMLElement): void {
    this.cards.set(id, el);
    this.add(el);
  }

  private text(target: HTMLElement): HTMLElement {
    return target.querySelector(".text") as HTMLElement;
  }

  private sessionEvent(data: SessionEventData): void {
    const p = data.payload ?? {};
    switch (data.type) {
      case "assistant.delta": {
        this.answer ??= this.message("assistant", "");
        this.answered = true;
        this.text(this.answer).textContent += String(p.text ?? "");
        break;
      }
      case "assistant.message": {
        if (this.answer) this.text(this.answer).textContent = String(p.text ?? "");
        else this.message("assistant", String(p.text ?? ""));
        this.answer = null;
        this.answered = true;
        break;
      }
      case "tool.call": {
        const el = h("div", { class: "tool", "data-tool": String(p.tool) }, h("span", { class: "tool-name" }, String(p.tool)), h("span", { class: "tool-state" }, "running"));
        this.tools.set(String(p.call_id), el);
        this.add(el);
        break;
      }
      case "tool.result":
        this.toolResult(p);
        break;
      case "subagent.spawned":
      case "subagent.progress":
      case "subagent.finished":
        this.subagent(data.type, p);
        break;
      default:
        break;
    }
  }

  /** A tool call ended: its row shows the outcome. */
  protected toolResult(p: Record<string, unknown>): void {
    const el = this.tools.get(String(p.call_id));
    const state = el?.querySelector(".tool-state");
    if (state) state.textContent = p.ok ? "done" : "failed";
  }

  private subagent(type: string, p: Record<string, unknown>): void {
    const id = String(p.subagent_id ?? "");
    let el = this.subagents.get(id);
    if (!el) {
      el = h("div", { class: "subagent", "data-subagent": id });
      this.subagents.set(id, el);
      this.add(el);
    }
    const what = type === "subagent.spawned" ? `Subagent started: ${String(p.goal ?? "")}` : type === "subagent.finished" ? `Subagent ${String(p.status ?? "finished")}: ${String(p.summary ?? "")}` : `Subagent: ${String(p.activity ?? "")}`;
    el.textContent = what;
  }

  private finished(data: TurnFinished): void {
    if (!this.answered && data.final_response) this.message("assistant", data.final_response);
    this.answer = null;
    this.answered = false;
    if (data.error) this.add(h("p", { class: "notice error", role: "alert" }, `${data.error.message}${data.error.hint ? ` (${data.error.hint})` : ""}`));
  }
}
