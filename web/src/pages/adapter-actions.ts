// The channel setup forms' actions (spec: web-dashboard, Channel setup
// forms): Test and Restart adapter, and the interactive pairing flow an
// adapter declares (WhatsApp QR linking, Telegram Create with QR). A flow
// runs in the daemon; its steps (each refreshed QR code as an image) come
// on the event stream as `adapter.pairing`, and closing the panel cancels
// it, so a late result is discarded and nothing is saved.

import { api, ApiError, profilePath } from "../lib/api";
import { h, notice } from "../lib/dom";
import { connected, follow } from "../lib/events";
import { canDrive } from "../lib/state";

export interface ActionTarget {
  platform: string;
  display: string;
  pairing: string | null;
}

interface PairingStep {
  platform?: string;
  state: string;
  image?: string;
  deep_link?: string;
  code?: string;
  account?: string | null;
  message?: string;
}

const PAIRING_LABEL: Record<string, string> = {
  whatsapp_qr: "Link with QR",
  telegram_create: "Create with QR",
};

function adapterPath(profile: string, platform: string, rest: string): string {
  return profilePath(profile, `/adapters/${encodeURIComponent(platform)}${rest}`);
}

function errorText(err: unknown): string {
  return err instanceof ApiError ? err.message : String(err);
}

/** Test and Restart adapter. */
export function actionButtons(profile: string, adapter: ActionTarget): HTMLElement {
  const status = h("span", { role: "status", class: "muted" });
  const run = (label: string, rest: string, report: (body: Record<string, unknown>) => string) => {
    const button = h("button", { type: "button", disabled: !canDrive() }, label);
    button.addEventListener("click", async () => {
      status.textContent = `${label}…`;
      try {
        status.textContent = report(await api.post<Record<string, unknown>>(adapterPath(profile, adapter.platform, rest)));
      } catch (err) {
        status.textContent = errorText(err);
      }
    });
    return button;
  };
  const test = run("Test", "/test", (b) => {
    if (b.ok !== true) return `Test failed: ${String(b.error)}`;
    return b.inbound_only ? "Test passed (inbound only)" : `Test passed in ${String(b.latency_ms)} ms`;
  });
  const restart = run("Restart adapter", "/restart", (b) => `Restarted: ${String(b.state)}${b.message ? ` (${String(b.message)})` : ""}`);
  return h("div", { class: "row adapter-actions" }, test, restart, status);
}

function showStep(step: PairingStep, image: HTMLImageElement, link: HTMLAnchorElement, status: HTMLElement): boolean {
  image.hidden = !step.image;
  if (step.image) image.src = step.image;
  link.hidden = !step.deep_link;
  if (step.deep_link) link.href = step.deep_link;
  const texts: Record<string, string> = {
    starting: "Starting…",
    qr: "Scan the code with your phone; it refreshes until it is linked.",
    code: `Enter the code ${step.code ?? ""} on your phone.`,
    linked: `Linked${step.account ? ` as ${step.account}` : ""}; the adapter restarted.`,
    cancelled: "Cancelled; nothing was saved.",
  };
  status.textContent = texts[step.state] ?? step.message ?? step.state;
  return !["starting", "qr", "code"].includes(step.state);
}

/** The adapter's pairing flow, when it declares one. */
export function pairingPanel(profile: string, adapter: ActionTarget): HTMLElement | null {
  if (!adapter.pairing) return null;
  const image = h("img", { class: "pairing-qr", alt: `${adapter.display} pairing QR code`, hidden: true });
  const link = h("a", { target: "_blank", rel: "noopener noreferrer", hidden: true }, "Open the link instead");
  const status = h("p", { role: "status", class: "muted" });
  const start = h("button", { type: "button", disabled: !canDrive() }, PAIRING_LABEL[adapter.pairing] ?? "Pair");
  const cancel = h("button", { type: "button", hidden: true }, "Cancel");
  let unfollow: (() => void) | null = null;
  const finish = () => {
    unfollow?.();
    unfollow = null;
    cancel.hidden = true;
    start.disabled = !canDrive();
  };
  start.addEventListener("click", async () => {
    start.disabled = true;
    cancel.hidden = false;
    unfollow = follow((event) => {
      if (!("event" in event) || event.event !== "adapter.pairing" || event.profile !== profile) return;
      const step = event.data as unknown as PairingStep;
      if (step.platform === adapter.platform && showStep(step, image, link, status)) finish();
    });
    // Shown before the request: the flow's first steps may arrive on the
    // event stream before its answer and must not be overwritten.
    showStep({ state: "starting" }, image, link, status);
    try {
      await connected();
      await api.post(adapterPath(profile, adapter.platform, "/pairing"));
    } catch (err) {
      status.textContent = errorText(err);
      finish();
    }
  });
  cancel.addEventListener("click", async () => {
    try {
      await api.del(adapterPath(profile, adapter.platform, "/pairing"));
    } catch (err) {
      status.replaceChildren(notice(errorText(err), "error"));
    }
    showStep({ state: "cancelled" }, image, link, status);
    finish();
  });
  return h("div", { class: "pairing", "data-pairing": adapter.pairing }, h("div", { class: "row" }, start, cancel), status, image, link);
}
