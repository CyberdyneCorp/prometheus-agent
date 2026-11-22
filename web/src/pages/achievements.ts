// The achievements page (spec: pets, Achievements from session history):
// tiered badges computed on the daemon's machine from the profile's
// session history, each with exactly what it counts. The page renders at
// once with what is known (`achievements.list`); while a scan runs in the
// background it shows a scanning indicator and asks again until the scan
// has finished. No achievement data leaves the machine.

import type { AchievementBadge, Achievements } from "../../../schema/rpc";
import { h, notice, page } from "../lib/dom";
import { rpc } from "../lib/rpc";
import { profileOf } from "../lib/state";

/** How often the page asks while a scan runs. */
const POLL_MS = 700;

export async function achievementsPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const status = h("p", { class: "scan-state", role: "status" });
  const grid = h("div", { class: "badges" });
  const view = page("Achievements", h("div", { class: "achievements-page" }, status, grid));
  const show = (listed: Achievements) => {
    status.textContent = listed.scanning
      ? `Scanning your session history… (${listed.sessions} sessions so far)`
      : `${listed.sessions} sessions analysed${listed.scanned_at ? ` · last scan ${new Date(listed.scanned_at).toLocaleString()}` : ""}`;
    status.dataset.scanning = String(listed.scanning);
    grid.replaceChildren(...listed.badges.map(badge));
  };
  const load = async (): Promise<void> => {
    try {
      const client = await rpc();
      const listed = (await client.call("achievements.list", { profile })) as Achievements;
      show(listed);
      if (listed.scanning && view.isConnected !== false) window.setTimeout(() => void load(), POLL_MS);
    } catch (err) {
      status.replaceChildren(notice((err as Error).message, "error"));
    }
  };
  // Not awaited: the page shows before the first answer.
  status.textContent = "Scanning your session history…";
  status.dataset.scanning = "true";
  void load();
  return view;
}

function badge(b: AchievementBadge): HTMLElement {
  const tier = b.tier ?? "locked";
  const next = b.next_tier && b.next_threshold !== null && b.next_threshold !== undefined ? `${b.value} / ${b.next_threshold} for ${b.next_tier}` : `${b.value}`;
  return h(
    "article",
    { class: `badge tier-${tier}`, "data-id": b.id, "data-tier": tier },
    h("h3", {}, b.name),
    h("p", { class: "tier" }, tier),
    h("p", { class: "count" }, next),
    h("p", { class: "what" }, b.description),
    h("p", { class: "category" }, b.category),
  );
}
