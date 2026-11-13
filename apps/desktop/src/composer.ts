// The chat composer (spec: desktop-app, Drafts are durable and never sent
// for the user; Composer history and queue editing; Slash palette from one
// command registry): the text and staged images of the chat it shows,
// kept as that chat's draft on every change; `Up`/`Down` in an empty
// composer recall earlier prompts; Stop or `Esc` interrupts. A restored
// draft only fills the composer: nothing is sent, opened or focused for
// the user.

import { h } from "../../../web/src/lib/dom";
import { type Draft, type Drafts, NEW_CHAT, type StagedImage } from "./drafts";
import type { PromptHistory } from "./history";
import { type PaletteEntry, suggest } from "./palette";
import { shell } from "./shell";

export class Composer {
  readonly input = h("textarea", { name: "prompt", rows: 3, placeholder: "Message the agent", "aria-label": "Message" });
  readonly strip = h("div", { class: "restore-strip" });
  readonly el: HTMLFormElement;
  private readonly suggestions = h("ul", { class: "palette", role: "listbox", "aria-label": "Commands" });
  private readonly staged = h("ul", { class: "staged", "aria-label": "Attachments" });
  private images: StagedImage[] = [];
  private chat = NEW_CHAT;
  private palette: PaletteEntry[] = [];
  onSend: (text: string, images: StagedImage[]) => void = () => {};
  onStop: () => void = () => {};

  constructor(
    private readonly drafts: Drafts,
    private readonly history: PromptHistory,
    extras: HTMLElement[],
  ) {
    const attach = h("button", { type: "button", name: "attach-image" }, "Attach image");
    attach.addEventListener("click", () => void this.stage());
    const stop = h("button", { type: "button", name: "stop" }, "Stop");
    stop.addEventListener("click", () => this.onStop());
    this.el = h("form", { class: "composer" }, this.suggestions, this.staged, this.input, ...extras, attach, h("button", { type: "submit" }, "Send"), stop);
    this.el.addEventListener("submit", (event) => {
      event.preventDefault();
      this.send();
    });
    this.input.addEventListener("input", () => {
      this.history.reset();
      this.save();
      this.showSuggestions();
    });
    this.input.addEventListener("keydown", (event) => this.key(event));
  }

  /** The session's command registry; what was typed is matched again. */
  setCatalog(catalog: PaletteEntry[]): void {
    this.palette = catalog;
    this.showSuggestions();
  }

  get catalog(): PaletteEntry[] {
    return this.palette;
  }

  /** Shows `chat`'s draft (the current one was saved on each change). */
  show(chat: string): void {
    this.chat = chat;
    const draft = this.drafts.get(chat);
    this.input.value = draft?.text ?? "";
    this.images = draft?.images ?? [];
    this.strip.replaceChildren();
    this.suggestions.replaceChildren();
    this.renderStaged();
  }

  /** The chat moved from the draft to its new session. */
  adopt(chat: string): void {
    this.drafts.take(this.chat);
    this.chat = chat;
    this.save();
  }

  /** A gone session's draft continues in this (fallback) chat, once,
   *  with Undo. */
  carry(from: Draft): void {
    const before: Draft = { text: this.input.value, images: this.images };
    this.input.value = before.text ? `${before.text}\n${from.text}` : from.text;
    this.images = [...before.images, ...from.images];
    this.renderStaged();
    this.save();
    const undo = h("button", { type: "button", name: "undo-restore" }, "Undo");
    undo.addEventListener("click", () => {
      this.input.value = before.text;
      this.images = before.images;
      this.renderStaged();
      this.save();
      this.strip.replaceChildren();
    });
    this.strip.replaceChildren(h("p", { class: "notice info", role: "status" }, "Restored your unsent message ", undo));
  }

  /** An image message of this chat that the daemon never acknowledged
   *  (the app closed first): offered once. */
  offerImageDraft(draft: Draft): void {
    const restore = h("button", { type: "button", name: "restore-image-draft" }, "Restore image draft");
    restore.addEventListener("click", () => {
      this.input.value = draft.text;
      this.images = draft.images;
      this.renderStaged();
      this.save();
      this.strip.replaceChildren();
    });
    this.strip.replaceChildren(h("p", { class: "notice info", role: "status" }, restore));
  }

  /** Annotation comments join the draft (text and snapshots); nothing
   *  is sent until the user presses Send. */
  attachComments(text: string, snapshots: string[]): void {
    this.input.value = this.input.value ? `${this.input.value}\n\n${text}` : text;
    this.images.push(...snapshots.map((data, i) => ({ name: `snapshot-${i + 1}.png`, data })));
    this.renderStaged();
    this.save();
  }

  /** A message that was not sent comes back, unless the user already
   *  typed something new. */
  putBack(draft: Draft): void {
    if (this.input.value !== "" || this.images.length > 0) return;
    this.input.value = draft.text;
    this.images = draft.images;
    this.renderStaged();
    this.save();
  }

  clear(): void {
    this.input.value = "";
    this.images = [];
    this.renderStaged();
    this.save();
  }

  private save(): void {
    this.drafts.set(this.chat, { text: this.input.value, images: this.images });
  }

  private send(): void {
    const text = this.input.value.trim();
    if (!text && this.images.length === 0) return;
    this.suggestions.replaceChildren();
    this.history.push(text);
    this.onSend(text, this.images);
  }

  private key(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      this.onStop();
      return;
    }
    if ((event.key !== "ArrowUp" && event.key !== "ArrowDown") || (this.input.value !== "" && !this.recalled())) return;
    const text = this.history.step(event.key === "ArrowUp");
    if (text === null) return;
    event.preventDefault();
    this.input.value = text;
    this.input.dataset.recalled = text;
  }

  /** The composer shows a recalled prompt, unchanged. */
  private recalled(): boolean {
    return this.input.dataset.recalled !== undefined && this.input.dataset.recalled === this.input.value;
  }

  private async stage(): Promise<void> {
    const picked = await shell<{ path: string | null }>("pick_file", { title: "Attach an image" }).catch(() => ({ path: null }));
    if (!picked.path) return;
    this.images.push({ name: picked.path.split(/[\\/]/).pop() ?? picked.path, path: picked.path });
    this.renderStaged();
    this.save();
  }

  private renderStaged(): void {
    this.staged.replaceChildren(
      ...this.images.map((image, i) => {
        const remove = h("button", { type: "button", name: "unstage", "aria-label": `Remove ${image.name}` }, "×");
        remove.addEventListener("click", () => {
          this.images.splice(i, 1);
          this.renderStaged();
          this.save();
        });
        return h("li", {}, image.name, remove);
      }),
    );
  }

  private showSuggestions(): void {
    const typed = this.input.value;
    const items = typed.startsWith("/") ? suggest(this.palette, typed) : [];
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
}
