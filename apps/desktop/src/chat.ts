// The chat pane (spec: desktop-app, Sessions are created with desktop
// source; Gone-session verdict; Slash palette from one command registry),
// rendered with the dashboard's transcript and request cards.

import type { ApprovalRequestParams, ClarifyRequestParams, Decision, Envelope, TranscriptMessage } from "../../../schema/rpc";
import { approvalCard } from "../../../web/src/components/approval";
import { clarifyCard } from "../../../web/src/components/clarify";
import { Transcript } from "../../../web/src/components/transcript";
import { h, notice } from "../../../web/src/lib/dom";
import type { ServerRequest } from "../../../web/src/lib/rpc";
import type { DesktopClient } from "./client";
import { type PaletteEntry, refusal, suggest } from "./palette";
import { resumeVerdict } from "./verdict";

interface Attached {
  epoch: string;
  latest_seq: number;
}

interface Dispatched {
  type: "done" | "send" | "refused";
  notice?: string | null;
  message?: string | null;
  reason?: string | null;
}

export class ChatView {
  readonly transcript = new Transcript();
  private readonly titleEl = h("h2", { class: "chat-title" }, "New chat");
  private readonly problems = h("div", { class: "problems" });
  private readonly input = h("textarea", { name: "prompt", rows: 3, placeholder: "Message the agent", "aria-label": "Message" });
  private readonly suggestions = h("ul", { class: "palette", role: "listbox", "aria-label": "Commands" });
  readonly el: HTMLElement;
  sessionId: string | null = null;
  /** A prompt of `sessionId` was submitted (a turn runs). */
  onSubmitted: (sessionId: string) => void = () => {};
  private cwd: string | null = null;
  private catalog: PaletteEntry[] = [];

  constructor(private readonly client: DesktopClient) {
    const form = h("form", { class: "composer" }, this.suggestions, this.input, h("button", { type: "submit" }, "Send"));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      void this.send();
    });
    this.input.addEventListener("input", () => this.showSuggestions());
    this.el = h("section", { class: "chat", "aria-label": "Chat" }, this.titleEl, this.transcript.el, this.problems, form);
  }

  /** The open session's title, when it has one. */
  title(): string | null {
    return this.sessionId ? this.titleEl.textContent : null;
  }

  /** An empty draft (a deep link never fills it in). */
  draft(cwd: string | null = null): void {
    if (this.sessionId) this.client.unwatch(this.sessionId);
    this.sessionId = null;
    this.cwd = cwd;
    this.catalog = [];
    this.titleEl.textContent = cwd ? `New chat in ${cwd}` : "New chat";
    this.el.dataset.session = "";
    this.transcript.load([]);
    this.problems.replaceChildren();
    this.input.value = "";
    this.suggestions.replaceChildren();
  }

  /** Opens `sessionId`; a failure becomes a draft only for a gone session. */
  async open(sessionId: string): Promise<void> {
    if (this.sessionId && this.sessionId !== sessionId) this.client.unwatch(this.sessionId);
    this.sessionId = sessionId;
    this.titleEl.textContent = `Session ${sessionId}`;
    this.el.dataset.session = sessionId;
    this.problems.replaceChildren();
    try {
      const attached = await this.client.call<Attached>("session.resume", { session_id: sessionId });
      this.client.watch(sessionId, attached.latest_seq, attached.epoch);
      await this.reload();
    } catch (err) {
      this.resumeFailed(sessionId, err);
    }
  }

  private resumeFailed(sessionId: string, err: unknown): void {
    if (resumeVerdict(err, this.client) === "draft") {
      this.draft();
      this.problems.replaceChildren(notice(`Session ${sessionId} no longer exists; this is a new chat.`, "warn"));
      return;
    }
    const retry = h("button", { type: "button", name: "retry" }, "Retry");
    retry.addEventListener("click", () => void this.open(sessionId));
    this.problems.replaceChildren(notice(`Could not open the session: ${(err as Error).message}`, "error"), retry);
  }

  /** Reloads the stored transcript and the command catalog. */
  async reload(): Promise<void> {
    if (!this.sessionId) return;
    const stored = await this.client.call<{ messages: TranscriptMessage[] }>("session.transcript", { session_id: this.sessionId });
    this.transcript.load(stored.messages);
    const catalog = await this.client.call<{ commands: PaletteEntry[] }>("commands.catalog", { session_id: this.sessionId });
    this.catalog = catalog.commands;
  }

  apply(envelope: Envelope): void {
    if (envelope.session_id && envelope.session_id === this.sessionId) this.transcript.apply(envelope);
  }

  request(request: ServerRequest): void {
    if (request.params.session_id !== this.sessionId) return;
    if (request.method === "approval.request") {
      const answer = async (decision: Decision) => {
        await this.client.call("approval.respond", { id: request.id, decision });
      };
      this.transcript.card(request.id, approvalCard(request.id, request.params as unknown as ApprovalRequestParams, answer, true));
    } else if (request.method === "clarify.request") {
      const answer = (answers: string[]) => this.client.answer(request.id, { answers });
      this.transcript.card(request.id, clarifyCard(request.params as unknown as ClarifyRequestParams, answer, true));
    }
  }

  private showSuggestions(): void {
    const typed = this.input.value;
    const items = typed.startsWith("/") ? suggest(this.catalog, typed) : [];
    this.suggestions.replaceChildren(
      ...items.map((item) => {
        const option = h("li", { role: "option", "data-command": item }, item);
        option.addEventListener("click", () => {
          this.input.value = `${item} `;
          this.input.focus();
          this.suggestions.replaceChildren();
        });
        return option;
      }),
    );
  }

  private async send(): Promise<void> {
    const text = this.input.value.trim();
    if (!text) return;
    this.problems.replaceChildren();
    this.suggestions.replaceChildren();
    try {
      if (text.startsWith("/") && (await this.command(text))) {
        this.input.value = "";
        return;
      }
      await this.ensureSession();
      this.transcript.message("user", text);
      this.input.value = "";
      await this.client.call("prompt.submit", { session_id: this.sessionId, text, files: [] });
      if (this.sessionId) this.onSubmitted(this.sessionId);
    } catch (err) {
      this.problems.replaceChildren(notice((err as Error).message, "error"));
    }
  }

  /** Creates the draft's session, with desktop source. */
  private async ensureSession(): Promise<void> {
    if (this.sessionId) return;
    const created = await this.client.call<{ session_id: string }>("session.create", {
      profile: this.client.profile,
      source: "desktop",
      cwd: this.cwd,
    });
    this.sessionId = created.session_id;
    this.el.dataset.session = created.session_id;
    this.titleEl.textContent = `Session ${created.session_id}`;
    this.client.watch(created.session_id, null, null);
    await this.client.call("session.attach", { session_id: created.session_id });
    const catalog = await this.client.call<{ commands: PaletteEntry[] }>("commands.catalog", { session_id: created.session_id });
    this.catalog = catalog.commands;
  }

  /** Runs a slash command; false when `text` is no command (a prompt). */
  private async command(text: string): Promise<boolean> {
    const [head = ""] = text.slice(1).split(/\s+/);
    if (!this.catalog.some((e) => e.name === head || (e.aliases ?? []).includes(head))) return false;
    const refused = refusal(this.catalog, text);
    if (refused) {
      this.problems.replaceChildren(notice(refused, "warn"));
      return true;
    }
    if (head === "new") {
      this.draft();
      return true;
    }
    const args = text.slice(1 + head.length).trim();
    const done = await this.client.call<Dispatched>("command.dispatch", { session_id: this.sessionId, name: head, args });
    if (done.type === "send" && done.message) {
      await this.client.call("prompt.submit", { session_id: this.sessionId, text: done.message, files: [] });
    } else {
      this.problems.replaceChildren(notice(done.notice ?? done.reason ?? "Done", done.type === "refused" ? "warn" : "info"));
    }
    return true;
  }
}
