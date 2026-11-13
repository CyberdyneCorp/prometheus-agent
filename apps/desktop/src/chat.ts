// The chat pane (spec: desktop-app, Sessions are created with desktop
// source; Gone-session verdict; Slash palette from one command registry;
// Chat transcript rendering; Drafts are durable and never sent for the
// user; Composer history and queue editing; Desktop status bar; Composer
// model picker): the transcript with its timeline and find bar, the queue
// above the composer, the model picker and the status bar. Each session
// keeps its distance from the bottom when the window leaves it.

import type { ApprovalRequestParams, ClarifyRequestParams, Decision, Envelope, FsFile, TranscriptMessage } from "../../../schema/rpc";
import { approvalCard } from "../../../web/src/components/approval";
import { clarifyCard } from "../../../web/src/components/clarify";
import { h, notice } from "../../../web/src/lib/dom";
import type { ServerRequest } from "../../../web/src/lib/rpc";
import type { DesktopClient } from "./client";
import { Composer } from "./composer";
import { Drafts, NEW_CHAT, type StagedImage } from "./drafts";
import { FindBar } from "./find";
import { PromptHistory } from "./history";
import { ModelPicker } from "./models";
import { type PaletteEntry, refusal } from "./palette";
import { QueueView } from "./queue";
import { StatusBar } from "./statusbar";
import { DesktopTranscript } from "./transcript";
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

/** How a window's chat pane is built. */
export interface ChatSetup {
  client: DesktopClient;
  /** The window's label: drafts are per window. */
  window: string;
  /** Where confirmations show. */
  dialogs: HTMLElement;
}

export class ChatView {
  readonly transcript = new DesktopTranscript();
  readonly status: StatusBar;
  readonly find: FindBar;
  private readonly titleEl = h("h2", { class: "chat-title" }, "New chat");
  private readonly problems = h("div", { class: "problems" });
  private readonly queue: QueueView;
  private readonly picker: ModelPicker;
  private readonly composer: Composer;
  private readonly drafts: Drafts;
  private readonly client: DesktopClient;
  /** Each session's distance from the bottom when the window left it. */
  private readonly scroll = new Map<string, number>();
  readonly el: HTMLElement;
  sessionId: string | null = null;
  /** A prompt of `sessionId` was submitted (a turn runs). */
  onSubmitted: (sessionId: string) => void = () => {};
  /** The window shows another chat (`null`: a draft); the panes follow. */
  onFollow: (sessionId: string | null) => void = () => {};
  /** Adds context to a prompt before it is sent (the HUD's window below). */
  decorate: ((text: string) => Promise<string>) | null = null;
  /** `/journey` (`/learning`, `/memory-graph`): the window opens the graph. */
  onJourney: () => void = () => {};
  private cwd: string | null = null;
  /** The open session's live output reached this window since the last
   *  turn ended. */
  private live = false;

  constructor(setup: ChatSetup) {
    this.client = setup.client;
    this.drafts = new Drafts(setup.window);
    this.queue = new QueueView(this.client);
    this.picker = new ModelPicker(this.client, setup.dialogs);
    this.status = new StatusBar(this.client, setup.dialogs);
    this.composer = new Composer(this.drafts, new PromptHistory(() => this.client.profile), [this.picker.el]);
    this.composer.onSend = (text, images) => void this.send(text, images);
    this.composer.onStop = () => void this.stop();
    this.find = new FindBar(this.transcript.el);
    this.transcript.fetchImage = (path) => this.image(path);
    this.transcript.source = () => this.stored();
    this.el = h(
      "section",
      { class: "chat", "aria-label": "Chat" },
      this.titleEl,
      this.find.el,
      this.transcript.frame,
      this.problems,
      this.composer.strip,
      this.queue.el,
      this.composer.el,
      this.status.el,
    );
  }

  /** The open session's title, when it has one. */
  title(): string | null {
    return this.sessionId ? this.titleEl.textContent : null;
  }

  private leave(): void {
    if (!this.sessionId) return;
    this.scroll.set(this.sessionId, this.transcript.distanceFromBottom());
    this.client.unwatch(this.sessionId);
  }

  /** An empty draft (a deep link never fills it in). */
  draft(cwd: string | null = null): void {
    this.leave();
    this.sessionId = null;
    this.cwd = cwd;
    this.composer.setCatalog([]);
    this.titleEl.textContent = cwd ? `New chat in ${cwd}` : "New chat";
    this.el.dataset.session = "";
    this.transcript.load([]);
    this.problems.replaceChildren();
    this.composer.show(NEW_CHAT);
    void this.queue.follow(null);
    void this.status.follow(null);
    this.onFollow(null);
    void this.picker.load(null);
  }

  /** Opens `sessionId`; a failure becomes a draft only for a gone session. */
  async open(sessionId: string): Promise<void> {
    if (this.sessionId !== sessionId) this.leave();
    this.sessionId = sessionId;
    this.titleEl.textContent = `Session ${sessionId}`;
    this.el.dataset.session = sessionId;
    this.problems.replaceChildren();
    this.composer.show(sessionId);
    // Another chat opened meanwhile (a deep link, a click): this one stops.
    const current = () => this.sessionId === sessionId;
    try {
      const attached = await this.client.call<Attached>("session.resume", { session_id: sessionId });
      if (!current()) return;
      this.client.watch(sessionId, attached.latest_seq, attached.epoch);
      await this.reload();
      if (!current()) return;
      const saved = this.scroll.get(sessionId);
      if (saved !== undefined) this.transcript.restore(saved);
      const unsent = this.drafts.takeUnacknowledged(sessionId);
      if (unsent) this.composer.offerImageDraft(unsent);
      await Promise.all([this.queue.follow(sessionId), this.status.follow(sessionId), this.picker.load(sessionId)]);
      if (current()) this.onFollow(sessionId);
    } catch (err) {
      if (current()) this.resumeFailed(sessionId, err);
    }
  }

  private resumeFailed(sessionId: string, err: unknown): void {
    if (resumeVerdict(err, this.client) === "draft") {
      const unsent = this.drafts.take(sessionId);
      this.draft();
      this.problems.replaceChildren(notice(`Session ${sessionId} no longer exists; this is a new chat.`, "warn"));
      if (unsent && (unsent.text || unsent.images.length > 0)) this.composer.carry(unsent);
      return;
    }
    const retry = h("button", { type: "button", name: "retry" }, "Retry");
    retry.addEventListener("click", () => void this.open(sessionId));
    this.problems.replaceChildren(notice(`Could not open the session: ${(err as Error).message}`, "error"), retry);
  }

  private async stored(): Promise<TranscriptMessage[]> {
    if (!this.sessionId) return [];
    return (await this.client.call<{ messages: TranscriptMessage[] }>("session.transcript", { session_id: this.sessionId })).messages;
  }

  /** Reloads the stored transcript and the command catalog. */
  async reload(): Promise<void> {
    const sessionId = this.sessionId;
    if (!sessionId) return;
    const messages = await this.stored();
    const catalog = await this.client.call<{ commands: PaletteEntry[] }>("commands.catalog", { session_id: sessionId });
    if (this.sessionId !== sessionId) return;
    this.transcript.load(messages);
    this.composer.setCatalog(catalog.commands);
  }

  /** Annotation comments for the composer (never sent by themselves). */
  attachComments(text: string, snapshots: string[]): void {
    this.composer.attachComments(text, snapshots);
  }

  /** Puts `el` (the preview suggestions) above the composer. */
  aboveComposer(el: HTMLElement): void {
    this.queue.el.before(el);
  }

  /** Re-reads what a reconnect may have changed (the daemon-held queue). */
  async reattached(): Promise<void> {
    if (this.sessionId) await this.queue.follow(this.sessionId);
    else await this.picker.load(null);
  }

  /** An image the session's tools made, read through its connection. */
  private async image(path: string): Promise<string> {
    if (!this.sessionId) throw new Error("no session");
    const file = await this.client.call<FsFile>("fs.read", { session_id: this.sessionId, path });
    if (file.kind !== "image" || !file.mime) throw new Error("not an image");
    return `data:${file.mime};base64,${file.data}`;
  }

  apply(envelope: Envelope): void {
    if (!envelope.session_id || envelope.session_id !== this.sessionId) return;
    if (envelope.event === "session.event") this.live = true;
    // The last event this window received of the session.
    this.el.dataset.lastSeq = String(envelope.seq);
    this.transcript.apply(envelope);
  }

  /** A turn of the open session ended: one the window joined midway
   *  (after a relaunch) renders whole from the store. */
  async turnFinished(sessionId: string): Promise<void> {
    if (sessionId !== this.sessionId) return;
    const joinedMidway = !this.live;
    this.live = false;
    if (joinedMidway) await this.reload();
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

  /** Stop or `Esc`: interrupts the turn and, with prompts queued, pauses
   *  the queue so they wait. */
  private async stop(): Promise<void> {
    if (!this.sessionId) return;
    try {
      if (this.queue.waiting) await this.queue.pause();
      await this.client.call("session.interrupt", { session_id: this.sessionId });
    } catch (err) {
      this.problems.replaceChildren(notice((err as Error).message, "error"));
    }
  }

  /** Sends what the composer held; the composer is free again at once
   *  (a failure puts the text back when nothing new was typed). */
  private async send(text: string, images: StagedImage[]): Promise<void> {
    this.problems.replaceChildren();
    this.composer.clear();
    try {
      if (text.startsWith("/") && (await this.command(text))) return;
      await this.ensureSession();
      const sessionId = this.sessionId ?? "";
      this.drafts.sending(sessionId, { text, images });
      const sent = this.decorate ? await this.decorate(text) : text;
      this.transcript.message("user", sent);
      await this.client.call("prompt.submit", { session_id: sessionId, text: sent, files: [], images: images.map((i) => (i.data ? { data: i.data } : { path: i.path ?? "" })) });
      // The chat is a session once its first prompt was admitted.
      this.el.dataset.session = sessionId;
      this.drafts.acknowledged(sessionId);
      this.onSubmitted(sessionId);
    } catch (err) {
      this.composer.putBack({ text, images });
      this.problems.replaceChildren(notice((err as Error).message, "error"));
    }
  }

  /** Creates the draft's session, with desktop source, on the model this
   *  device picked last (the profile's default is not changed). */
  private async ensureSession(): Promise<void> {
    if (this.sessionId) return;
    const pick = this.picker.pick;
    const created = await this.client.call<{ session_id: string }>("session.create", {
      profile: this.client.profile,
      source: "desktop",
      cwd: this.cwd,
      setup: pick ? { model: pick } : {},
    });
    this.sessionId = created.session_id;
    this.composer.adopt(created.session_id);
    this.titleEl.textContent = `Session ${created.session_id}`;
    this.client.watch(created.session_id, null, null);
    await this.client.call("session.attach", { session_id: created.session_id });
    await this.picker.applyPrefs(created.session_id, pick);
    const catalog = await this.client.call<{ commands: PaletteEntry[] }>("commands.catalog", { session_id: created.session_id });
    this.composer.setCatalog(catalog.commands);
    void this.queue.follow(created.session_id);
    void this.status.follow(created.session_id);
    void this.picker.load(created.session_id);
    this.onFollow(created.session_id);
  }

  /** Runs a slash command; false when `text` is no command (a prompt). */
  private async command(text: string): Promise<boolean> {
    const catalog = this.composer.catalog;
    const [head = ""] = text.slice(1).split(/\s+/);
    // The memory graph is the window's own view of `/journey`.
    if (["journey", "learning", "memory-graph"].includes(head)) {
      this.onJourney();
      return true;
    }
    if (!catalog.some((e) => e.name === head || (e.aliases ?? []).includes(head))) return false;
    const refused = refusal(catalog, text);
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
