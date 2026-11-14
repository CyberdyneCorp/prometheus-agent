// `/pairing` (spec: web-dashboard, Built-in pages; messaging-gateway, DM
// pairing): the profile's DM pairings, pending ones first. A sender who
// messaged an adapter got a one-time code in that chat; the owner approves
// them by typing that code here (`POST pairing/{code} {platform}`), as with
// `prometheus gateway pairing approve`. The code is never listed: only the
// sender has it, so a pairing cannot be approved without them. Observers
// see the list read-only.

import { api, ApiError, profilePath } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { canDrive, profileOf } from "../lib/state";

interface Pairing {
  platform: string;
  sender_id: string;
  sender_name: string | null;
  approved: boolean;
  requested_at: string;
  expires_at: string | null;
}

function sender(p: Pairing): string {
  return p.sender_name ? `${p.sender_name} (${p.sender_id})` : p.sender_id;
}

/** The code field and Approve button of a pending pairing. */
function approveForm(profile: string, p: Pairing, done: (said: string) => void, failed: (said: string) => void): HTMLFormElement {
  const drive = canDrive();
  const form = h(
    "form",
    { class: "approve", "aria-label": `Approve ${sender(p)} on ${p.platform}`, "data-sender": p.sender_id },
    h("input", { name: "code", placeholder: "code from the chat", required: true, autocomplete: "off", disabled: !drive }),
    " ",
    h("button", { type: "submit", name: "approve", disabled: !drive }, "Approve"),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const code = String(new FormData(form).get("code") ?? "").trim();
    try {
      await api.post(profilePath(profile, `/pairing/${encodeURIComponent(code)}`), { platform: p.platform });
      done(`Approved ${sender(p)} on ${p.platform}`);
    } catch (err) {
      failed(err instanceof ApiError ? err.message : String(err));
    }
  });
  return form;
}

export async function pairingPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const status = h("div", { class: "status", role: "status" });
  const list = h("div", { class: "pairings" });
  const reload = async (): Promise<void> => {
    delete list.dataset.ready;
    try {
      const { pairings } = await api.get<{ pairings: Pairing[] }>(profilePath(profile, "/pairing"));
      const ok = (said: string) => {
        status.replaceChildren(notice(said));
        void reload();
      };
      const bad = (said: string) => status.replaceChildren(notice(said, "error"));
      const pending = pairings.filter((p) => !p.approved);
      const approved = pairings.filter((p) => p.approved);
      list.replaceChildren(
        h("h2", {}, "Pending"),
        pending.length
          ? table(["Platform", "Sender", "Requested", "Expires", ""], pending.map((p) => [p.platform, sender(p), p.requested_at, p.expires_at ?? "", approveForm(profile, p, ok, bad)]))
          : notice("No pending pairing requests."),
        h("h2", {}, "Approved"),
        approved.length ? table(["Platform", "Sender", "Requested"], approved.map((p) => [p.platform, sender(p), p.requested_at])) : notice("No approved senders yet."),
      );
    } catch (err) {
      list.replaceChildren(notice((err as Error).message, "error"));
    }
    list.dataset.ready = "true";
  };
  await reload();
  const hint = canDrive() ? null : notice("You are an observer: approving needs a driver.", "info");
  return page("Pairing", h("p", { class: "muted" }, `Profile ${profile}`), hint, status, list);
}
