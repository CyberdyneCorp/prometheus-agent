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
  /** The install (macOS app, AppImage) offers "Install command-line tool". */
  cli_link?: boolean;
  quick_ask: { hotkey: string };
  hud?: { snap_hotkey: string; tap_to_summon: boolean };
}

interface CliInstall {
  ok: boolean;
  installed?: { link: string; target: string; on_path: boolean };
  reason?: string;
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
    hudSettings(rows, options, (key, value) => save(key, value, status)),
    updates(),
    ...(options.cli_link ? [cliTool()] : []),
    h("details", { class: "platform" }, h("summary", {}, "Platform support"), h("ul", {}, ...rows.map((r) => h("li", { "data-feature": r.feature, "data-state": r.state }, `${r.feature}: ${r.state} (${r.reason})`)))),
  );
}

/** HUD settings: a placement feature the platform cannot offer (native
 *  Wayland) is disabled with its reason. */
function hudSettings(rows: FeatureSupport[], options: LaunchOptions, save: (key: string, value: unknown) => void): HTMLElement {
  const hud = options.hud ?? { snap_hotkey: "CommandOrControl+Shift+G", tap_to_summon: false };
  const snapReason = unavailable(rows, "desktop.hud.snap");
  const snap = h("input", { name: "desktop.hud.snap_hotkey", value: hud.snap_hotkey, "aria-label": "Snap to pointer", disabled: snapReason !== null });
  snap.addEventListener("change", () => save("desktop.hud.snap_hotkey", snap.value.trim()));
  const tap = h("input", { type: "checkbox", name: "desktop.hud.tap_to_summon", checked: hud.tap_to_summon, "aria-label": "Summon with a tap on the tray icon" });
  tap.addEventListener("change", () => save("desktop.hud.tap_to_summon", tap.checked));
  const belowReason = unavailable(rows, "desktop.hud.window_below");
  return h(
    "section",
    { class: "hud-settings", "aria-label": "HUD" },
    h("h4", {}, "HUD"),
    h("label", {}, "Snap to pointer ", snap),
    snapReason ? h("p", { class: "reason", "data-for": "desktop.hud.snap_hotkey" }, snapReason) : "",
    h("label", {}, tap, " Summon with a tap on the tray icon"),
    belowReason ? h("p", { class: "reason", "data-for": "desktop.hud.window_below" }, `Window context: ${belowReason}`) : "",
  );
}

/** "Install command-line tool": links ~/.local/bin/prometheus to the CLI
 *  this app bundles (deb, rpm and MSIX installs put it on PATH themselves). */
function cliTool(): HTMLElement {
  const status = h("p", { class: "cli-status", role: "status" });
  const install = h("button", { type: "button", name: "install-cli" }, "Install command-line tool");
  install.addEventListener("click", async () => {
    install.disabled = true;
    const outcome = await shell<CliInstall>("install_cli");
    install.disabled = false;
    if (!outcome.installed) {
      status.textContent = outcome.reason ?? "The command-line tool was not installed.";
      return;
    }
    const where = outcome.installed.link;
    status.textContent = outcome.installed.on_path ? `Linked ${where}: run prometheus in a terminal.` : `Linked ${where}. Add ~/.local/bin to your PATH to run prometheus in a terminal.`;
  });
  return h("section", { class: "cli-tool", "aria-label": "Command-line tool" }, h("h4", {}, "Command-line tool"), install, status);
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
