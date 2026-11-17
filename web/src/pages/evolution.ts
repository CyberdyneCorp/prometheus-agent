// The harness-evolution view (spec: web-dashboard, Harness-evolution view):
// the profile's evolution runs (mode, budget, rollouts and improver sessions
// planned, cost, status) and, for one run, each Propose-Probe-Compose stage,
// the candidate harness diffs against the baseline, the blind selector's
// pick and the proposals, which the user adopts or discards. Adoption applies
// from the next session (`evolve.proposals.adopt` without `now`), so live
// sessions keep their cached prompt. Everything goes over the client
// protocol (`evolve.list`, `evolve.status`, `evolve.proposals.adopt|reject`).

import type { EvolveJobRow, EvolveJobs, EvolveReply } from "../../../schema/rpc";
import { h, notice, page, table } from "../lib/dom";
import { RpcError, rpc } from "../lib/rpc";
import { canDrive, profileOf } from "../lib/state";

type Item = Record<string, unknown>;

/** The reason shown when the daemon does not serve a method. */
function failure(err: unknown, method: string): HTMLElement {
  const unserved = err instanceof RpcError && err.failure.data?.code === "rpc.not_served";
  return unserved
    ? notice(`This daemon does not serve ${method} yet, so there is nothing to show.`, "warn")
    : notice((err as Error).message, "error");
}

const text = (v: unknown): string => (v === null || v === undefined ? "" : String(v));

export async function evolutionPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const body = h("div", { class: "evolution-page" });
  const view = page("Harness evolution", body);
  const job = params.get("job");
  try {
    body.append(...(job ? await detail(profile, job) : await runs(profile)));
  } catch (err) {
    body.replaceChildren(failure(err, job ? "evolve.status" : "evolve.list"));
  }
  body.dataset.ready = "true";
  return view;
}

async function runs(profile: string): Promise<HTMLElement[]> {
  const client = await rpc();
  const listed = (await client.call("evolve.list", { profile })) as EvolveJobs;
  if (listed.jobs.length === 0) return [notice("No evolution runs yet.")];
  const link = (row: EvolveJobRow): HTMLElement => {
    const url = new URL(location.href);
    url.searchParams.set("job", row.id);
    return h("a", { href: url.toString(), "data-job": row.id }, row.id);
  };
  const budget = (row: EvolveJobRow): string => `k=${row.k}${row.budget === null || row.budget === undefined ? "" : `, $${row.budget}`}`;
  return [
    table(
      ["Run", "Mode", "Budget", "Rollouts", "Improver sessions", "Cost (USD)", "Status"],
      listed.jobs.map((row) => [link(row), row.mode, budget(row), row.rollouts, row.sessions ?? 0, row.cost_usd.toFixed(4), row.status]),
    ),
  ];
}

async function detail(profile: string, job: string): Promise<HTMLElement[]> {
  const client = await rpc();
  const status = (await client.call("evolve.status", { profile, job_id: job })) as EvolveReply;
  const data = status.data as Item;
  const summary = (data.summary ?? {}) as Item;
  const modes = (summary.modes ?? []) as Item[];
  const out: HTMLElement[] = [h("h2", {}, `Run ${job}`), notice(status.message)];
  for (const mode of modes) out.push(...rounds(mode));
  const candidates = (data.candidates ?? []) as Item[];
  if (candidates.length) {
    out.push(h("h2", {}, "Candidate harnesses (against the baseline)"));
    for (const c of candidates) out.push(diffBlock(`${text(c.mode)} ${text(c.label)}: ${text(c.summary)}`, text(c.diff), text(c.label)));
  }
  out.push(...proposals(profile, (data.proposals ?? []) as Item[]));
  return out;
}

/** One mode's stages (the Propose-Probe-Compose rounds) and the selector's pick. */
function rounds(mode: Item): HTMLElement[] {
  const stages = (mode.stages ?? []) as Item[];
  const selected = mode.selected as Item | null | undefined;
  const pick = selected
    ? h("p", { class: "selected", "data-mode": text(mode.mode) }, `Selected ${text(selected.label)} (${text(selected.id)}) by ${text(selected.selected_by)}${selected.reason ? `: ${text(selected.reason)}` : ""}`)
    : notice("No candidate selected.");
  return [
    h("h3", {}, `Mode ${text(mode.mode)} (${text(mode.status)})`),
    table(
      ["Stage", "Attempts", "Kept base", "Fell back", "Cached", "Cost (USD)", "Error"],
      stages.map((s) => [text(s.label), text(s.attempts), s.kept_base ? "yes" : "no", s.fell_back ? "yes" : "no", s.cached ? "yes" : "no", Number(s.usd ?? 0).toFixed(4), text(s.error)]),
    ),
    pick,
  ];
}

function diffBlock(title: string, diff: string, key: string): HTMLElement {
  return h("details", { class: "diff", "data-candidate": key }, h("summary", {}, title), h("pre", {}, diff || "(no change)"));
}

/** The run's proposals, each adoptable or discardable while pending. */
function proposals(profile: string, rows: Item[]): HTMLElement[] {
  if (rows.length === 0) return [h("h2", {}, "Proposals"), notice("This run made no proposal.")];
  return [h("h2", {}, "Proposals"), ...rows.map((p) => proposal(profile, p))];
}

function proposal(profile: string, p: Item): HTMLElement {
  const id = text(p.id);
  const result = h("div", { class: "result" });
  const box = h(
    "div",
    { class: "proposal", "data-proposal": id, "data-status": text(p.status) },
    h("h3", {}, `${id} (${text(p.status)})`),
    h("p", {}, `${text(p.mode)} final ${text(p.label)} on ${text(p.base_version)}: ${text(p.diff_summary)}`),
    p.approval_required ? notice("Adopting it adds or changes scripts, hooks or subagents.", "warn") : null,
    h("pre", { class: "proposal-diff" }, text(p.diff) || "(no change)"),
    result,
  );
  if (p.status !== "pending") return box;
  const act = async (method: string, verb: string): Promise<void> => {
    try {
      const client = await rpc();
      const reply = (await client.call(method, { profile, proposal_id: id })) as EvolveReply;
      box.dataset.status = verb;
      buttons.forEach((b) => (b.disabled = true));
      const tail = verb === "adopted" ? " It applies from the next session; sessions already open keep their prompt." : "";
      result.replaceChildren(notice(`${reply.message}.${tail}`));
    } catch (err) {
      result.replaceChildren(notice((err as Error).message, "error"));
    }
  };
  const adopt = h("button", { type: "button", name: "adopt", disabled: !canDrive() }, "Adopt");
  const discard = h("button", { type: "button", name: "discard", disabled: !canDrive() }, "Discard");
  adopt.addEventListener("click", () => void act("evolve.proposals.adopt", "adopted"));
  discard.addEventListener("click", () => void act("evolve.proposals.reject", "rejected"));
  const buttons = [adopt, discard];
  box.insertBefore(h("div", { class: "actions" }, adopt, " ", discard), result);
  return box;
}
