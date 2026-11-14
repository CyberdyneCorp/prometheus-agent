// A small DOM helper: elements from tag, attributes and children. Text is
// always set as text (never parsed as HTML), so nothing the daemon or a
// model sends can inject markup.

export type Child = Node | string | number | null | undefined | false;
export type Attrs = Record<string, string | number | boolean | EventListener | null | undefined>;

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (typeof value === "function") {
      el.addEventListener(name.replace(/^on/, ""), value);
    } else if (value === true) {
      el.setAttribute(name, "");
    } else {
      el.setAttribute(name, String(value));
    }
  }
  append(el, ...children);
  return el;
}

export function append(parent: Node, ...children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    parent.appendChild(typeof child === "object" ? child : document.createTextNode(String(child)));
  }
}

export function clear(el: Node): void {
  while (el.firstChild) el.removeChild(el.firstChild);
}

/** A section with a heading, the shape of every page. */
export function page(title: string, ...children: Child[]): HTMLElement {
  return h("section", { class: "page", "aria-label": title }, h("h1", {}, title), ...children);
}

/** A notice line (`role="status"`). */
export function notice(text: string, kind: "info" | "error" | "warn" = "info"): HTMLElement {
  return h("p", { class: `notice ${kind}`, role: kind === "error" ? "alert" : "status" }, text);
}

/** A table from a header row and body rows. */
export function table(head: string[], rows: Child[][]): HTMLTableElement {
  return h(
    "table",
    {},
    h("thead", {}, h("tr", {}, ...head.map((c) => h("th", {}, c)))),
    h("tbody", {}, ...rows.map((r) => h("tr", {}, ...r.map((c) => h("td", {}, c))))),
  );
}
