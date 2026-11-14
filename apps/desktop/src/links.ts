// Windows never open themselves (spec: desktop-app, Windows never open
// themselves; External URL scheme allowlist). Page content cannot open a
// window, and a link leaves the app only after a trusted primary-button
// click, through the shell's external URL command, which accepts only
// `http:`, `https:` and `mailto:` (local paths go to the guarded opener;
// every other scheme answers `{ok: false, reason: "invalid"}`).

import { type Opened, shell } from "./shell";

/** The last external URL answer, shown to the user when refused. */
export let lastOpened: Opened | null = null;

export function guardLinks(onRefused: (href: string, reason: string) => void): void {
  // The native webview denies new windows too; this keeps page code from
  // even trying.
  window.open = () => null;
  document.addEventListener(
    "click",
    (event) => {
      const link = (event.target as Element | null)?.closest?.("a[href]");
      if (!(link instanceof HTMLAnchorElement)) return;
      const href = link.getAttribute("href") ?? "";
      if (href.startsWith("#")) return;
      event.preventDefault();
      if (!event.isTrusted || event.button !== 0) return;
      void shell<Opened>("open_external", { url: href }).then((opened) => {
        lastOpened = opened;
        if (!opened.ok) onRefused(href, opened.reason ?? "refused");
      });
    },
    true,
  );
}
