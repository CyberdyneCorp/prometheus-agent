// The desktop generate flow (spec: pets, Generating a pet): the same two
// paid steps as `/hatch`, each shown with its estimated cost and run only
// after Yes; the drafts are shown to pick one, remix or retry. A profile
// without a reference-capable image backend gets the error and spends
// nothing.

import { h } from "../../../../web/src/lib/dom";
import type { DesktopClient } from "../client";

interface HatchStep {
  flow: string;
  text: string;
  done: boolean;
  failed?: boolean;
  drafts?: string[];
  slug?: string | null;
}

export async function hatchDialog(client: DesktopClient, host: HTMLElement, description: string, hatched: () => void): Promise<void> {
  const profile = client.profile;
  const text = h("p", { class: "hatch-text", role: "status" });
  const drafts = h("div", { class: "hatch-drafts" });
  const actions = h("div", { class: "row" });
  const box = h("div", { class: "hatch-dialog", role: "dialog", "aria-label": "Generate a pet" }, h("h3", {}, "Generate a pet"), text, drafts, actions);
  host.append(box);
  const button = (label: string, name: string, run: () => void) => {
    const b = h("button", { type: "button", name }, label);
    b.addEventListener("click", run);
    return b;
  };
  const close = button("Close", "close", () => box.remove());
  const show = (step: HatchStep) => {
    text.textContent = step.text;
    box.dataset.state = step.done ? (step.failed ? "failed" : "done") : "asking";
    drafts.replaceChildren(
      ...(step.drafts ?? []).map((url, n) =>
        h("button", { type: "button", name: `draft-${n + 1}`, class: "draft", onclick: () => void answer(step.flow, String(n + 1)) }, h("img", { src: url, alt: `Draft ${n + 1}` })),
      ),
    );
    if (step.done) {
      if (step.slug) hatched();
      return actions.replaceChildren(close);
    }
    const tweak = h("input", { type: "text", name: "remix", placeholder: "Remix with…", "aria-label": "Remix" });
    const picking = (step.drafts ?? []).length > 0;
    actions.replaceChildren(
      ...(picking
        ? [tweak, button("Remix", "remix-go", () => void answer(step.flow, `remix ${tweak.value}`)), button("Retry", "retry", () => void answer(step.flow, "retry"))]
        : [button("Yes", "yes", () => void answer(step.flow, "y"))]),
      button("Cancel", "cancel", () => void answer(step.flow, "cancel")),
    );
  };
  const answer = async (flow: string, said: string) => {
    for (const b of actions.querySelectorAll("button")) b.disabled = true;
    text.textContent = "Working…";
    try {
      show(await client.call<HatchStep>("pets.hatch.answer", { profile, flow, answer: said }));
    } catch (err) {
      show({ flow, text: (err as Error).message, done: true, failed: true });
    }
  };
  try {
    show(await client.call<HatchStep>("pets.hatch.start", { profile, description }));
  } catch (err) {
    show({ flow: "", text: (err as Error).message, done: true, failed: true });
  }
}
