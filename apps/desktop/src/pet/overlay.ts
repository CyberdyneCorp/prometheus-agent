// The pop-out pet (spec: pets, Desktop pet and pop-out overlay): a
// transparent, always-on-top window (`#pet`) with no protocol connection
// of its own. The main window sends it the pet and its state; a click
// opens a mini composer whose prompt goes back to the main window, which
// submits it to the most recent session; a double-click shows or hides
// the main window; pressing and holding moves the window, whose place
// the shell keeps.

import { h } from "../../../../web/src/lib/dom";
import { onShellEvent, shell } from "../shell";
import { Sprite, type SpriteData } from "./sprite";
import type { PetState } from "./state";

type FromMain =
  | { type: "sprite"; data: SpriteData | null; scale: number }
  | { type: "state"; state: PetState; bubble: string; mail: boolean; heart: boolean; scale: number };

const HOLD_MS = 250;

function toMain(message: unknown): void {
  void shell("pet_relay", { to: "main", message }).catch((err: unknown) => console.warn("pet relay:", err));
}

export function petOverlay(root: HTMLElement): void {
  document.documentElement.dataset.window = "pet";
  const sprite = new Sprite("Pet");
  const speech = h("div", { class: "pet-bubble", hidden: true });
  const mail = h("span", { class: "pet-mail", hidden: true, "aria-label": "A turn finished" }, "✉");
  const heart = h("span", { class: "pet-heart", hidden: true }, "♥");
  const input = h("input", { type: "text", name: "pet-prompt", "aria-label": "Ask the agent", placeholder: "Ask…", autocomplete: "off" });
  const composer = h("form", { class: "pet-composer", hidden: true }, input);
  const back = h("button", { type: "button", name: "pet-back", title: "Back into the window" }, "↩");
  const body = h("div", { class: "pet-body" }, mail, heart, sprite.el);
  root.replaceChildren(h("div", { class: "pet-overlay" }, speech, body, composer, back));

  onShellEvent("pet-message", (payload) => {
    const message = payload as FromMain;
    if (message.type === "sprite") {
      sprite.scale = message.scale;
      void sprite.load(message.data);
      return;
    }
    sprite.scale = message.scale;
    sprite.state = message.state;
    speech.textContent = message.bubble;
    speech.hidden = message.bubble === "";
    mail.hidden = !message.mail;
    heart.hidden = !message.heart;
  });

  let held = 0;
  let dragged = false;
  body.addEventListener("pointerdown", () => {
    dragged = false;
    held = window.setTimeout(() => {
      dragged = true;
      void shell("pet_drag").catch(() => {});
    }, HOLD_MS);
  });
  body.addEventListener("pointerup", () => window.clearTimeout(held));
  body.addEventListener("click", () => {
    if (dragged) return;
    composer.hidden = !composer.hidden;
    if (!composer.hidden) input.focus();
  });
  body.addEventListener("dblclick", () => {
    composer.hidden = true;
    mail.hidden = true;
    void shell("main_window", { action: "toggle" }).catch(() => {});
  });
  composer.addEventListener("submit", (e) => {
    e.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    toMain({ type: "submit", text });
    input.value = "";
    composer.hidden = true;
  });
  back.addEventListener("click", () => toMain({ type: "popin" }));
  toMain({ type: "ready" });
}
