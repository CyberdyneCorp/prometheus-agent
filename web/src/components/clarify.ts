// The clarify card (shared with the desktop): the agent's questions, each
// answered as text (or one of its choices), sent as the server request's
// answer.

import type { ClarifyRequestParams } from "../../../schema/rpc";
import { h } from "../lib/dom";

interface Question {
  question?: string;
  prompt?: string;
  choices?: string[];
  options?: string[];
}

export function clarifyCard(params: ClarifyRequestParams, answer: (answers: string[]) => void, canAnswer: boolean): HTMLElement {
  const inputs: HTMLInputElement[] = [];
  const fields = (params.questions as Question[]).map((q, i) => {
    const choices = q.choices ?? q.options ?? [];
    const list = `clarify-${i}-${Math.random().toString(36).slice(2)}`;
    const input = h("input", { type: "text", list: choices.length ? list : null, disabled: !canAnswer, "aria-label": q.question ?? q.prompt ?? `Question ${i + 1}` });
    inputs.push(input);
    return h(
      "label",
      { class: "field" },
      q.question ?? q.prompt ?? `Question ${i + 1}`,
      input,
      choices.length ? h("datalist", { id: list }, ...choices.map((c) => h("option", { value: c }))) : null,
    );
  });
  const status = h("p", { class: "card-status", role: "status" });
  const send = h("button", { type: "button", disabled: !canAnswer }, "Answer");
  send.addEventListener("click", () => {
    send.disabled = true;
    answer(inputs.map((i) => i.value));
    status.textContent = "Answered";
  });
  return h("article", { class: "card clarify", "aria-label": "Question from the agent" }, h("h3", {}, "The agent asks"), ...fields, send, status);
}
