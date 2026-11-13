// `/usage`: cost and tokens from the usage ledger, attributed to root
// session lineages (children beneath their root), with filters, daily
// totals, the cache-hit ratio, budget bars and CSV export (spec:
// web-dashboard, Cost and usage view (upgrade)).

import { api, profilePath } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { href } from "../lib/router";
import { profileOf } from "../lib/state";

interface Usage {
  cost_usd: number;
  input_tokens: number;
  output_tokens: number;
  cache_hit_ratio: number;
  calls: number;
}

interface Lineage {
  root_session_id: string;
  title: string | null;
  total: Usage;
  own: Usage;
  children: { session_id: string; title: string | null; usage: Usage }[];
}

interface Report {
  total: Usage;
  lineages: Lineage[];
  daily: { date: string; usage: Usage }[];
  budgets: { scope: string; limit: number; spent: number; percent: number }[];
}

const usd = (v: number) => `$${v.toFixed(2)}`;

function budgetBar(b: Report["budgets"][number]): HTMLElement {
  const label = b.scope === "monthly_usd" ? "Monthly budget" : "Daily budget";
  return h(
    "div",
    { class: "budget", "data-budget": b.scope },
    h("span", {}, `${label}: ${usd(b.spent)} of ${usd(b.limit)}`),
    h("meter", { min: 0, max: 100, value: Math.min(b.percent, 100), high: 80, optimum: 0 }),
    h("span", { class: "percent" }, `${b.percent}% used`),
  );
}

function lineageRows(lineages: Lineage[]): HTMLElement {
  const body = h("tbody");
  for (const l of lineages) {
    body.append(h("tr", { class: "root", "data-root": l.root_session_id }, h("td", {}, l.title ?? l.root_session_id), h("td", { "data-total": l.root_session_id }, usd(l.total.cost_usd)), h("td", {}, usd(l.own.cost_usd)), h("td", {}, l.total.calls)));
    for (const c of l.children) {
      body.append(h("tr", { class: "child", "data-child-of": l.root_session_id }, h("td", {}, `↳ ${c.title ?? c.session_id}`), h("td", {}, usd(c.usage.cost_usd)), h("td", {}, ""), h("td", {}, c.usage.calls)));
    }
  }
  return h("table", {}, h("thead", {}, h("tr", {}, h("th", {}, "Session"), h("th", {}, "Total"), h("th", {}, "Own turns"), h("th", {}, "Calls"))), body);
}

export async function usagePage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const filters = new URLSearchParams();
  for (const key of ["from", "to", "model", "provider", "source"]) {
    const value = params.get(key);
    if (value) filters.set(key, value);
  }
  const query = filters.toString() ? `?${filters}` : "";
  let report: Report;
  try {
    report = await api.get<Report>(profilePath(profile, `/usage${query}`));
  } catch (err) {
    return page("Usage", notice((err as Error).message, "error"));
  }
  const field = (name: string, label: string, type = "text") => h("label", {}, label, h("input", { name, type, value: params.get(name) ?? "" }));
  const form = h("form", { class: "toolbar", method: "get", action: href("/usage") }, h("input", { type: "hidden", name: "profile", value: profile }), field("from", "From", "date"), field("to", "To", "date"), field("model", "Model"), field("provider", "Provider"), field("source", "Source"), h("button", { type: "submit" }, "Filter"));
  return page(
    "Usage",
    form,
    h("p", { class: "totals" }, `Total ${usd(report.total.cost_usd)} · ${report.total.calls} calls · cache hits ${(report.total.cache_hit_ratio * 100).toFixed(1)}%`),
    ...report.budgets.map(budgetBar),
    h("h2", {}, "By session"),
    report.lineages.length ? lineageRows(report.lineages) : notice("No usage in this range."),
    h("h2", {}, "By day"),
    table(["Day", "Cost", "Calls"], report.daily.map((d) => [d.date, usd(d.usage.cost_usd), d.usage.calls])),
    h("p", {}, h("a", { href: href(profilePath(profile, `/usage.csv${query}`)), download: "usage.csv" }, "Export CSV")),
  );
}
