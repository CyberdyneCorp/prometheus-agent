// Annotate mode, the app's side (spec: desktop-app, Annotate mode in the
// in-app browser): the agent in the preview frame reports each pick
// (already redacted in the page); the user types a note and saves a pin.
// Pins are numbered once and keep their number when another is deleted;
// saving one never submits anything. `Add N comments` attaches to the
// composer each pin's snapshot, selector, markup and styles, grouped by
// top-level subtree beyond 5 pins; switching chats clears the pins.

import { h } from "../../../web/src/lib/dom";

/** What the agent reports for a pick. */
export interface Pick {
  rect: { x: number; y: number; width: number; height: number };
  selector: string;
  top: string;
  markup: string;
  styles: Record<string, string>;
  snapshot: string | null;
}

export interface Pin extends Pick {
  n: number;
  note: string;
}

/** Comments as attached to the composer: text and snapshot images. */
export interface Comments {
  text: string;
  /** PNG data URLs, in pin order. */
  images: string[];
}

/** Beyond this many pins, comments are grouped by top-level subtree. */
const GROUP_FROM = 6;

function comment(pin: Pin): string {
  const styles = Object.entries(pin.styles)
    .filter(([, v]) => v !== "")
    .map(([k, v]) => `${k}: ${v}`)
    .join("; ");
  return `Comment ${pin.n}: ${pin.note}\nSelector: ${pin.selector}\nMarkup:\n\`\`\`html\n${pin.markup}\n\`\`\`\nStyles: ${styles}`;
}

/** The composer text of `pins`. */
export function commentsText(pins: Pin[]): string {
  if (pins.length < GROUP_FROM) return pins.map(comment).join("\n\n");
  const groups = new Map<string, Pin[]>();
  for (const pin of pins) groups.set(pin.top, [...(groups.get(pin.top) ?? []), pin]);
  return [...groups].map(([top, group]) => `## In ${top}\n\n${group.map(comment).join("\n\n")}`).join("\n\n");
}

export class Annotator {
  readonly el = h("section", { class: "annotations", "aria-label": "Annotations", hidden: true });
  readonly toggle = h("button", { type: "button", name: "annotate", "aria-pressed": "false" }, "Annotate");
  private readonly list = h("ol", { class: "pins" });
  private readonly add = h("button", { type: "button", name: "add-comments", hidden: true }, "Add 0 comments");
  private readonly draft = h("div", { class: "pin-draft" });
  private pins: Pin[] = [];
  private next = 1;
  private active = false;

  constructor(
    private readonly frame: () => HTMLIFrameElement | null,
    readonly attach: (comments: Comments) => void,
  ) {
    this.toggle.addEventListener("click", () => this.setActive(!this.active));
    this.add.addEventListener("click", () => {
      this.attach({ text: commentsText(this.pins), images: this.pins.flatMap((p) => (p.snapshot ? [p.snapshot] : [])) });
      this.clear();
    });
    this.el.append(this.draft, this.list, this.add);
    window.addEventListener("message", (event) => {
      const source = this.frame()?.contentWindow;
      const data = event.data as ({ prometheus?: string } & Pick) | null;
      if (!source || event.source !== source || data?.prometheus !== "annotate.pick") return;
      this.offer(data);
    });
  }

  private setActive(on: boolean): void {
    this.active = on;
    this.toggle.setAttribute("aria-pressed", String(on));
    this.el.hidden = !on && this.pins.length === 0;
    this.frame()?.contentWindow?.postMessage({ prometheus: "annotate", action: on ? "start" : "stop" }, "*");
  }

  /** A pick waits for its note; Save makes it a pin. */
  private offer(pick: Pick): void {
    const note = h("input", { name: "pin-note", placeholder: "Note", "aria-label": "Note" });
    const save = h("button", { type: "button", name: "save-pin" }, "Save pin");
    const cancel = h("button", { type: "button", name: "cancel-pin" }, "Cancel");
    save.addEventListener("click", () => {
      this.pins.push({ ...pick, n: this.next++, note: note.value });
      this.draft.replaceChildren();
      this.render();
    });
    cancel.addEventListener("click", () => this.draft.replaceChildren());
    this.draft.replaceChildren(h("p", {}, pick.selector), note, save, cancel);
    note.focus();
  }

  private render(): void {
    this.el.hidden = !this.active && this.pins.length === 0;
    this.list.replaceChildren(
      ...this.pins.map((pin) => {
        const remove = h("button", { type: "button", name: "delete-pin", "aria-label": `Delete pin ${pin.n}` }, "×");
        remove.addEventListener("click", () => {
          this.pins = this.pins.filter((p) => p !== pin);
          this.render();
        });
        return h("li", { class: "pin", "data-pin": pin.n }, h("strong", {}, String(pin.n)), ` ${pin.note} `, h("code", {}, pin.selector), remove);
      }),
    );
    this.add.hidden = this.pins.length === 0;
    this.add.textContent = `Add ${this.pins.length} comment${this.pins.length === 1 ? "" : "s"}`;
  }

  /** Drops every pin (a chat switch, Close). */
  clear(): void {
    this.pins = [];
    this.next = 1;
    this.draft.replaceChildren();
    this.render();
  }
}
