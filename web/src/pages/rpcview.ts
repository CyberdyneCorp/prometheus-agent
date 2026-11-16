// Pages whose capability arrives in a later part of this release.

import { h, notice, page } from "../lib/dom";

/** Pages whose capability arrives in a later part of this release. */
export function pendingPage(title: string, what: string): () => HTMLElement {
  return () => page(title, notice(`${what} is not available in this build yet.`, "warn"), h("p", {}, "Use the CLI meanwhile: prometheus --help."));
}
