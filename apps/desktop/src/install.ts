// The install consent flow (spec: desktop-app, Install deep links open a
// consent flow). A `prometheus://plugins|skills/install?id=` link only
// opens this dialog: it shows what the configured catalog says about the
// entry (name, publisher, version, signature, capabilities), and the
// entry is installed only when the user clicks Install. Anything that is
// not a catalog id is refused before anything is asked or downloaded.

import { h, notice } from "../../../web/src/lib/dom";
import { shell } from "./shell";

export interface InstallLink {
  open: "install_plugin" | "install_skill";
  id: string;
  extra_params?: boolean;
}

interface Entry {
  name: string;
  description: string;
  publisher: string;
  version: string;
  signature: string;
  capabilities: string[];
}

/** Opens the dialog for `link` in `host`. */
export async function installDialog(link: InstallLink, profile: string, host: HTMLElement): Promise<void> {
  const kind = link.open === "install_plugin" ? "plugin" : "skill";
  const dialog = h("section", { class: "install-dialog", role: "dialog", "aria-label": "Install from the catalog" });
  host.replaceChildren(dialog);
  const close = h("button", { type: "button", name: "close" }, "Close");
  close.addEventListener("click", () => dialog.remove());
  if (link.extra_params) {
    dialog.replaceChildren(notice(`Unknown catalog entry ${link.id}`, "error"), close);
    return;
  }
  let entry: Entry;
  try {
    entry = await shell<Entry>("catalog_entry", { kind, id: link.id, profile });
  } catch (err) {
    dialog.replaceChildren(notice(String(err), "error"), close);
    return;
  }
  const status = h("p", { class: "install-status", role: "status" });
  const install = h("button", { type: "button", name: "install" }, "Install");
  const cancel = h("button", { type: "button", name: "cancel" }, "Cancel");
  cancel.addEventListener("click", () => dialog.remove());
  install.addEventListener("click", async () => {
    install.disabled = true;
    status.textContent = `Installing ${entry.name}…`;
    try {
      await shell("catalog_install", { kind, id: link.id, profile });
      status.textContent = `Installed ${entry.name}`;
    } catch (err) {
      status.textContent = `Not installed: ${String(err)}`;
      install.disabled = false;
    }
  });
  dialog.replaceChildren(
    h("h2", {}, `Install ${kind} ${entry.name}?`),
    h("p", {}, entry.description),
    h(
      "dl",
      {},
      h("dt", {}, "Publisher"),
      h("dd", { "data-field": "publisher" }, entry.publisher),
      h("dt", {}, "Version"),
      h("dd", { "data-field": "version" }, entry.version),
      h("dt", {}, "Signature"),
      h("dd", { "data-field": "signature" }, entry.signature),
      h("dt", {}, "Requested capabilities"),
      h("dd", {}, h("ul", { class: "capabilities" }, ...entry.capabilities.map((c) => h("li", {}, c)))),
    ),
    h("p", { class: "muted" }, `It is installed into profile ${profile}.`),
    install,
    cancel,
    status,
  );
}
