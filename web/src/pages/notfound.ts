import { h, page } from "../lib/dom";
import { href } from "../lib/router";

export function notFoundPage(): HTMLElement {
  return page("Page not found", h("p", {}, "There is no such page. "), h("a", { href: href("/sessions") }, "Go to sessions"));
}
