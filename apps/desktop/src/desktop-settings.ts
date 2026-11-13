// The desktop part of Settings (spec: desktop-app, Honest platform
// support in settings; Signed in-app updates; Development builds have no
// updater; Global hotkey quick-ask; Closing windows does not stop the
// agent). The platform rows are the daemon's (`platform_support` from
// `rpc.hello`) and the shell's own probes; a control whose feature is
// unavailable here is disabled with the reason instead of failing at use.

import type { FeatureSupport } from "../../../schema/rpc";
import { h } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { shell } from "./shell";

interface LaunchOptions {
  keep_in_tray: boolean;
  quick_ask: { hotkey: string };
}

interface UpdateOutcome {
  mechanism: "none" | "signed";
  ok: boolean;
  available?: { version: string; notes: string };
  staged?: string;
  message?: string;
}

function unavailable(rows: FeatureSupport[], feature: string): string | null {
  const row = rows.find((r) => r.feature === feature);
  return row && row.state === "unsupported" ? row.reason : null;
}

/** Renders the section into `host` (again after each reconnect). */
export async function desktopSettings(client: DesktopClient, host: HTMLElement): Promise<void> {
  const shellRows = (await shell<{ features: FeatureSupport[] }>("platform_support")).features;
  const rows = [...(client.hello?.platform_support.features ?? []), ...shellRows];
  const options = await shell<LaunchOptions>("launch_options");
  const save = async (key: string, value: unknown, status: HTMLElement) => {
    try {
      await client.call("config.patch", { profile: client.profile, changes: [{ key, value }] });
      status.textContent = "Saved; applies the next time the app starts";
    } catch (err) {
      status.textContent = (err as Error).message;
    }
  };
  const status = h("p", { class: "muted", role: "status" });
  const hotkeyReason = unavailable(rows, "desktop.global_shortcut");
  const hotkey = h("input", { name: "desktop.quick_ask.hotkey", value: options.quick_ask.hotkey, "aria-label": "Quick ask hotkey", disabled: hotkeyReason !== null });
  hotkey.addEventListener("change", () => void save("desktop.quick_ask.hotkey", hotkey.value.trim(), status));
  const trayReason = unavailable(rows, "desktop.tray");
  const tray = h("input", { type: "checkbox", name: "desktop.keep_in_tray", checked: options.keep_in_tray, "aria-label": "Keep running in the tray", disabled: trayReason !== null });
  tray.addEventListener("change", () => void save("desktop.keep_in_tray", tray.checked, status));
  const reason = (text: string | null, key: string) => (text ? [h("p", { class: "reason", "data-for": key }, text)] : []);
  host.replaceChildren(
    h("h3", {}, "Desktop"),
    h("label", {}, "Quick ask hotkey ", hotkey),
    ...reason(hotkeyReason, "desktop.quick_ask.hotkey"),
    h("label", {}, tray, " Keep running in the tray when the window closes"),
    ...reason(trayReason, "desktop.keep_in_tray"),
    status,
    updates(),
    h("details", { class: "platform" }, h("summary", {}, "Platform support"), h("ul", {}, ...rows.map((r) => h("li", { "data-feature": r.feature, "data-state": r.state }, `${r.feature}: ${r.state} (${r.reason})`)))),
  );
}

function updates(): HTMLElement {
  const status = h("p", { class: "update-status", role: "status" });
  const check = h("button", { type: "button", name: "check-update" }, "Check for updates");
  const apply = h("button", { type: "button", name: "apply-update", hidden: true }, "Download and install");
  const restart = h("button", { type: "button", name: "restart-app", hidden: true }, "Restart now");
  const show = (outcome: UpdateOutcome) => {
    status.textContent = outcome.available ? `Prometheus ${outcome.available.version} is available. ${outcome.available.notes}` : (outcome.message ?? "");
    if (!outcome.available && outcome.message) status.textContent = outcome.message;
    apply.hidden = !outcome.available || outcome.staged === outcome.available.version;
    restart.hidden = !outcome.staged;
  };
  check.addEventListener("click", async () => show(await shell<UpdateOutcome>("check_update")));
  apply.addEventListener("click", async () => {
    apply.disabled = true;
    const outcome = await shell<UpdateOutcome>("apply_update");
    apply.disabled = false;
    status.textContent = outcome.message ?? "";
    restart.hidden = !outcome.staged;
    if (outcome.staged) apply.hidden = true;
  });
  restart.addEventListener("click", () => void shell("restart_app"));
  return h("section", { class: "updates", "aria-label": "Updates" }, h("h4", {}, "Updates"), check, apply, restart, status);
}
