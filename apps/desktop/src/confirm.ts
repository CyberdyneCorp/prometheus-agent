// An in-window confirmation (the webview never opens native dialogs on
// the agent's behalf): resolves true on Continue, false on Cancel.

import { h } from "../../../web/src/lib/dom";

export function confirmIn(host: HTMLElement, text: string, action = "Continue"): Promise<boolean> {
  return new Promise((resolve) => {
    const yes = h("button", { type: "button", name: "confirm" }, action);
    const no = h("button", { type: "button", name: "cancel" }, "Cancel");
    const box = h("div", { class: "confirm", role: "alertdialog", "aria-label": "Confirm" }, h("p", {}, text), yes, no);
    const done = (answer: boolean) => {
      box.remove();
      resolve(answer);
    };
    yes.addEventListener("click", () => done(true));
    no.addEventListener("click", () => done(false));
    host.append(box);
    yes.focus();
  });
}
