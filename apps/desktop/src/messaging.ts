// The Messaging pane (spec: desktop-app, Messaging pane shows saved versus
// running state; Telegram quick setup by QR): each adapter of the chosen
// profile in exactly the state the daemon reports. After any credential
// save, clear or enable toggle a Restart adapter banner stays until the
// adapter has actually restarted (with Retry when the restart failed).
// Telegram offers Create with QR: the daemon's pairing flow, polled until
// it ends; closing the dialog cancels it and a late result is discarded.

import type { AdapterList, AdapterRow } from "../../../schema/rpc";
import { h } from "../../../web/src/lib/dom";
import type { ProfileClients } from "./profiles";

/** How often the open QR dialog asks for the flow's step. */
const POLL_MS = 1000;

interface Step {
  state: string;
  image?: string;
  deep_link?: string;
  message?: string;
  account?: string;
}

export class MessagingPane {
  readonly el = h("section", { class: "messaging", "aria-label": "Messaging" });
  /** Adapters whose saved configuration has not been restarted into. */
  private readonly unrestarted = new Map<string, string | null>();
  private profile = "";

  constructor(
    private readonly pool: ProfileClients,
    private readonly dialogs: HTMLElement,
  ) {}

  async show(profile: string): Promise<void> {
    this.profile = profile;
    delete this.el.dataset.ready;
    let rows: AdapterRow[] = [];
    try {
      rows = (await this.pool.call<AdapterList>(this.profile, "adapters.list", { profile })).adapters;
    } catch (err) {
      // No daemon: every adapter is stopped with it.
      this.el.dataset.daemon = "stopped";
      this.el.replaceChildren(h("h3", {}, "Messaging"), h("p", { class: "notice warn" }, `daemon_stopped: ${(err as Error).message}`));
      this.el.dataset.ready = "true";
      return;
    }
    delete this.el.dataset.daemon;
    this.el.replaceChildren(h("h3", {}, "Messaging"), ...rows.map((row) => this.row(row)));
    this.el.dataset.ready = "true";
  }

  private row(row: AdapterRow): HTMLElement {
    const state = h("span", { class: "adapter-state", "data-state": row.state }, row.reason ? `${row.state}: ${row.reason}` : row.state);
    const enable = h("input", { type: "checkbox", name: "adapter-enabled", checked: row.enabled, "aria-label": `Enable ${row.display}` });
    enable.addEventListener("change", () => void this.save(row.platform, { enabled: enable.checked }));
    const secrets = Object.entries(row.secrets).map(([name, set]) => {
      const input = h("input", { type: "password", name, placeholder: set ? "stored" : "not set", "aria-label": name });
      const store = h("button", { type: "button", name: "save-secret", "data-secret": name }, "Save");
      store.addEventListener("click", () => void this.save(row.platform, { secrets: { [name]: input.value } }));
      const clear = h("button", { type: "button", name: "clear-secret", "data-secret": name, disabled: !set }, "Clear");
      clear.addEventListener("click", () => void this.save(row.platform, { clear: [name] }));
      return h("label", {}, `${name} `, input, store, clear);
    });
    const qr = row.platform === "telegram" && row.pairing ? this.qrButton() : null;
    return h(
      "article",
      { class: "adapter", "data-platform": row.platform },
      h("h4", {}, row.display, " ", state),
      h("label", {}, enable, " Enabled"),
      ...secrets,
      qr,
      this.banner(row.platform),
    );
  }

  /** The Restart adapter banner, while a saved change waits for it. */
  private banner(platform: string): HTMLElement | null {
    if (!this.unrestarted.has(platform)) return null;
    const failed = this.unrestarted.get(platform);
    const restart = h("button", { type: "button", name: "restart-adapter" }, failed ? "Retry" : "Restart adapter");
    restart.addEventListener("click", () => void this.restart(platform));
    return h("div", { class: "restart-banner", role: "status" }, failed ? `Restart failed: ${failed} ` : "Saved; restart the adapter to use it. ", restart);
  }

  private async save(platform: string, change: Record<string, unknown>): Promise<void> {
    try {
      await this.pool.call(this.profile, "adapters.save", { profile: this.profile, platform, ...change });
      this.unrestarted.set(platform, null);
    } catch (err) {
      this.unrestarted.set(platform, (err as Error).message);
    }
    await this.show(this.profile);
  }

  private async restart(platform: string): Promise<void> {
    try {
      const row = await this.pool.call<AdapterRow>(this.profile, "adapters.restart", { profile: this.profile, platform });
      if (row.state === "error") this.unrestarted.set(platform, row.reason ?? "the adapter did not start");
      else this.unrestarted.delete(platform);
    } catch (err) {
      this.unrestarted.set(platform, (err as Error).message);
    }
    await this.show(this.profile);
  }

  private qrButton(): HTMLElement {
    const button = h("button", { type: "button", name: "create-with-qr" }, "Create with QR");
    button.addEventListener("click", () => void this.createWithQr());
    return button;
  }

  /** Telegram Create with QR: shows the daemon flow's steps until it ends. */
  private async createWithQr(): Promise<void> {
    const ref = { profile: this.profile, platform: "telegram" };
    const view = h("div", { class: "qr-step", role: "status" }, "Starting…");
    const close = h("button", { type: "button", name: "close-qr" }, "Close");
    const dialog = h("div", { class: "qr-dialog", role: "dialog", "aria-label": "Create a Telegram bot" }, h("h3", {}, "Create a Telegram bot"), view, close);
    let open = true;
    close.addEventListener("click", () => {
      open = false;
      dialog.remove();
      void this.pool.call(this.profile, "adapters.pairing.cancel", ref).catch(() => {});
    });
    this.dialogs.append(dialog);
    try {
      await this.pool.call(this.profile, "adapters.pairing.start", ref);
    } catch (err) {
      view.textContent = (err as Error).message;
      return;
    }
    while (open) {
      const step = await this.pool.call<Step>(this.profile, "adapters.pairing.state", ref).catch((): Step => ({ state: "failed" }));
      if (!open) return;
      this.render(view, step);
      if (!["starting", "qr", "pending"].includes(step.state)) break;
      await new Promise((resolve) => window.setTimeout(resolve, POLL_MS));
    }
    if (open) await this.show(this.profile);
  }

  private render(view: HTMLElement, step: Step): void {
    view.dataset.state = step.state;
    if (step.state === "qr" && step.image) {
      const link = step.deep_link ? h("a", { href: step.deep_link, class: "deep-link" }, step.deep_link) : "";
      view.replaceChildren(h("img", { src: step.image, alt: "Scan with Telegram", class: "qr" }), h("p", {}, "Scan the code with your phone, or open: "), link);
    } else if (step.state === "linked") {
      view.replaceChildren(`Telegram bot @${step.account ?? ""} is ready.`);
    } else {
      view.replaceChildren(step.message ?? step.state);
    }
  }
}
