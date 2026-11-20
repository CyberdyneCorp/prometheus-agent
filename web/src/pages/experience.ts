// The experience library view (spec: web-dashboard, Experience library
// view): every domain with its stored lessons and the harnesses adopted with
// them, the task each lesson came from (its title and evolution run), and
// per lesson Disable (hidden from proposers and new sessions until restored)
// and Delete. Over the client protocol: `experience.domains`,
// `experience.list` (with `all`, so disabled and rolled-back lessons stay
// visible here), `experience.disable`, `experience.restore`,
// `experience.forget`.

import type { ExperienceDomains, ExperienceEntries } from "../../../schema/rpc";
import { h, notice, page } from "../lib/dom";
import { RpcError, rpc } from "../lib/rpc";
import { canDrive, profileOf } from "../lib/state";

type Entry = Record<string, unknown>;

const text = (v: unknown): string => (v === null || v === undefined ? "" : String(v));

export async function experiencePage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const body = h("div", { class: "experience-page" });
  const view = page("Experience library", body);
  const load = async (): Promise<void> => {
    delete body.dataset.ready;
    try {
      const client = await rpc();
      const domains = (await client.call("experience.domains", { profile })) as ExperienceDomains;
      const listed = (await client.call("experience.list", { profile, all: true })) as ExperienceEntries;
      body.replaceChildren(...render(profile, domains, listed.entries as Entry[], load));
    } catch (err) {
      const unserved = err instanceof RpcError && err.failure.data?.code === "rpc.not_served";
      body.replaceChildren(
        unserved ? notice("This daemon does not serve experience.list yet, so there is nothing to show.", "warn") : notice((err as Error).message, "error"),
      );
    }
    body.dataset.ready = "true";
  };
  await load();
  return view;
}

function render(profile: string, domains: ExperienceDomains, entries: Entry[], reload: () => Promise<void>): HTMLElement[] {
  const names = new Set([...domains.domains.map((d) => d.name), ...entries.map((e) => text(e.domain))]);
  if (names.size === 0 || entries.length === 0) return [notice("No lessons yet: they are written when an evolved harness is adopted.")];
  return [...names].sort().map((name) => domain(profile, name, entries.filter((e) => e.domain === name), reload));
}

function domain(profile: string, name: string, entries: Entry[], reload: () => Promise<void>): HTMLElement {
  const harnesses = [...new Set(entries.map((e) => text(e.harness_version)).filter(Boolean))];
  return h(
    "section",
    { class: "domain", "data-domain": name },
    h("h2", {}, name),
    h("p", { class: "harnesses" }, harnesses.length ? `Adopted harnesses: ${harnesses.join(", ")}` : "No adopted harness."),
    entries.length ? h("ul", { class: "lessons" }, ...entries.map((e) => lesson(profile, e, reload))) : notice("No lessons in this domain."),
  );
}

function lesson(profile: string, e: Entry, reload: () => Promise<void>): HTMLElement {
  const id = text(e.entry_id);
  const status = text(e.status);
  const result = h("span", { class: "result" });
  const act = async (method: string): Promise<void> => {
    try {
      const client = await rpc();
      await client.call(method, { profile, entry_id: id });
      await reload();
    } catch (err) {
      result.replaceChildren(notice((err as Error).message, "error"));
    }
  };
  const button = (name: string, label: string, method: string): HTMLButtonElement => {
    const b = h("button", { type: "button", name, disabled: !canDrive() }, label);
    b.addEventListener("click", () => void act(method));
    return b;
  };
  const toggle = status === "active" ? button("disable", "Disable", "experience.disable") : button("restore", "Restore", "experience.restore");
  return h(
    "li",
    { class: "lesson", "data-entry": id, "data-status": status },
    h("p", { class: "cue" }, h("strong", {}, text(e.cue)), ` (${text(e.confidence)} confidence, ${status}, shown ${text(e.times_shown)} times)`),
    h("p", { class: "text" }, text(e.lesson)),
    h("p", { class: "source" }, `From task "${text(e.title)}" (run ${text(e.job_id)}, proposal ${text(e.proposal_id)}), harness ${text(e.harness_version)}`),
    h("div", { class: "actions" }, toggle, " ", button("delete", "Delete", "experience.forget"), " ", result),
  );
}
