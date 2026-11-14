// `/status`: daemon version, uptime, clients, running sessions, adapter
// states and the platform support matrix (spec: web-dashboard, Built-in
// pages; Honest platform support matrix).

import { api } from "../lib/api";
import { h, page, table } from "../lib/dom";

interface Feature {
  feature: string;
  state: string;
  reason: string;
}

interface Status {
  version: string;
  build_id: string;
  uptime_secs: number;
  clients: number;
  sessions: number;
  active_turns: number;
  adapters: { profile: string; platform: string; state: string; message: string | null }[];
  platform: { target: string; tier: string; features: Feature[] };
}

function uptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const hrs = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return `${d ? `${d}d ` : ""}${hrs}h ${m}m ${secs % 60}s`;
}

export function matrix(features: Feature[]): HTMLElement {
  return table(
    ["Feature", "Support", "Why"],
    features.map((f) => [f.feature, h("span", { class: `state ${f.state}`, "data-feature": f.feature }, f.state), f.reason]),
  );
}

export async function statusPage(): Promise<HTMLElement> {
  const s = await api.get<Status>("/api/status");
  return page(
    "Status",
    h(
      "dl",
      { class: "facts" },
      h("dt", {}, "Version"),
      h("dd", { "data-fact": "version" }, `${s.version} (${s.build_id.slice(0, 12)})`),
      h("dt", {}, "Uptime"),
      h("dd", { "data-fact": "uptime" }, uptime(s.uptime_secs)),
      h("dt", {}, "Attached clients"),
      h("dd", { "data-fact": "clients" }, s.clients),
      h("dt", {}, "Running sessions"),
      h("dd", { "data-fact": "sessions" }, s.sessions),
      h("dt", {}, "Turns running"),
      h("dd", {}, s.active_turns),
      h("dt", {}, "Platform"),
      h("dd", {}, `${s.platform.target} (${s.platform.tier})`),
    ),
    h("h2", {}, "Adapters"),
    s.adapters.length
      ? table(["Profile", "Platform", "State", "Detail"], s.adapters.map((a) => [a.profile, a.platform, a.state, a.message ?? ""]))
      : h("p", { class: "muted", "data-empty": "adapters" }, "No messaging adapters are enabled."),
    h("h2", {}, "Platform support"),
    matrix(s.platform.features),
  );
}
