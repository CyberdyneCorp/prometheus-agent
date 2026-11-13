// `/sessions`: the profile's sessions, full-text search, deletion, and a
// pending-approval badge from the live event stream.

import { api, profilePath } from "../lib/api";
import { clear, h, notice, page, table } from "../lib/dom";
import { follow } from "../lib/events";
import { href } from "../lib/router";
import { canDrive, profileOf } from "../lib/state";

interface Row {
  id: string;
  title: string | null;
  source: string;
  started_at: string;
  message_count: number;
}

const pending = new Map<string, Set<string>>();
let following = false;

/** Approval requests waiting per session, kept for every page. */
function trackApprovals(): void {
  if (following) return;
  following = true;
  follow((event) => {
    if ("truncated" in event) return;
    const session = event.session_id ?? "";
    const id = String(event.data.id ?? "");
    if (event.event === "approval.request") {
      if (!pending.has(session)) pending.set(session, new Set());
      pending.get(session)?.add(id);
    } else if (event.event === "request.settled") {
      for (const ids of pending.values()) ids.delete(id);
    } else return;
    document.dispatchEvent(new CustomEvent("approvals-changed"));
  });
}

function badge(session: string): HTMLElement | null {
  const waiting = pending.get(session)?.size ?? 0;
  return waiting ? h("span", { class: "badge", "data-badge": "approval" }, "approval pending") : null;
}

export async function sessionsPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  trackApprovals();
  const list = h("div", { class: "list" });
  const query = h("input", { type: "search", name: "q", placeholder: "Search messages", "aria-label": "Search", value: params.get("q") ?? "" });
  const load = async () => {
    clear(list);
    try {
      if (query.value.trim()) {
        const found = await api.get<{ hits: { session_id: string; title: string | null; snippet: string }[] }>(profilePath(profile, `/sessions?q=${encodeURIComponent(query.value)}`));
        list.append(table(["Session", "Match"], found.hits.map((hit) => [h("a", { href: href(`/chat?profile=${profile}&session=${hit.session_id}`) }, hit.title ?? hit.session_id), hit.snippet])));
        return;
      }
      const data = await api.get<{ sessions: Row[] }>(profilePath(profile, "/sessions"));
      const rows = data.sessions.map((s) => [
        h("a", { href: href(`/chat?profile=${profile}&session=${s.id}`), "data-session": s.id }, s.title ?? s.id),
        s.source,
        s.started_at,
        s.message_count,
        h("span", { class: "badges", "data-badges": s.id }, badge(s.id)),
        canDrive() ? h("button", { type: "button", class: "danger", onclick: () => void remove(s.id) }, "Delete") : "",
      ]);
      list.append(rows.length ? table(["Session", "Source", "Started", "Messages", "", ""], rows) : notice("No sessions yet."));
    } catch (err) {
      list.append(notice((err as Error).message, "error"));
    }
  };
  const remove = async (id: string) => {
    if (!window.confirm(`Delete session ${id}? This cannot be undone.`)) return;
    try {
      await api.del(profilePath(profile, `/sessions/${id}`));
      await load();
    } catch (err) {
      list.prepend(notice((err as Error).message, "error"));
    }
  };
  document.addEventListener("approvals-changed", () => {
    for (const cell of Array.from(list.querySelectorAll<HTMLElement>("[data-badges]"))) {
      cell.replaceChildren(...[badge(cell.dataset.badges ?? "")].filter((b): b is HTMLElement => b !== null));
    }
  });
  const search = h("form", { class: "toolbar" }, query, h("button", { type: "submit" }, "Search"));
  search.addEventListener("submit", (event) => {
    event.preventDefault();
    void load();
  });
  await load();
  return page("Sessions", h("p", {}, h("a", { href: href(`/chat?profile=${profile}`) }, "New chat")), search, list);
}
