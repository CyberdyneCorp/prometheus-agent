// The approval card (shared with the desktop): what the agent wants to run
// and why, answered with `approval.respond` over the client protocol.

import type { ApprovalRequestParams, Decision } from "../../../schema/rpc";
import { h } from "../lib/dom";

export function approvalCard(
  id: string,
  params: ApprovalRequestParams,
  answer: (decision: Decision) => Promise<void>,
  canAnswer: boolean,
): HTMLElement {
  const status = h("p", { class: "card-status", role: "status" });
  const buttons: HTMLButtonElement[] = [];
  const choose = (decision: Decision) => async () => {
    for (const b of buttons) b.disabled = true;
    try {
      await answer(decision);
      status.textContent = decision === "deny" ? "Denied" : `Approved (${decision})`;
    } catch (err) {
      status.textContent = `Not sent: ${(err as Error).message}`;
      for (const b of buttons) b.disabled = false;
    }
  };
  const labels: [Decision, string][] = [
    ["once", "Approve once"],
    ["session", "Approve for this session"],
    ["always", "Always approve"],
    ["deny", "Deny"],
  ];
  for (const [decision, label] of labels) {
    buttons.push(h("button", { type: "button", "data-decision": decision, disabled: !canAnswer, onclick: choose(decision) }, label));
  }
  return h(
    "article",
    { class: "card approval", "data-request": id, "aria-label": "Approval request" },
    h("h3", {}, `Approve ${params.tool}?`),
    params.subject ? h("pre", { class: "subject" }, params.subject) : null,
    h("ul", {}, ...params.findings.map((f) => h("li", {}, `${f.reason} (${f.detector})`))),
    h("div", { class: "actions" }, ...buttons),
    canAnswer ? null : h("p", { class: "muted" }, "Observers cannot answer approvals."),
    status,
  );
}
