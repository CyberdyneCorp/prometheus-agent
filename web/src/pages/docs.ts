// `/docs` (spec: web-dashboard, Embedded documentation): the user guide
// and generated reference of the running build, embedded in the binary
// and searched offline (`/api/docs/search`). `?key=<config key>` shows
// that key's reference entry; messages naming a key link here
// (`docLink`). Pages render as text: nothing is fetched or parsed as HTML.

import { api } from "../lib/api";
import { h, notice, page } from "../lib/dom";
import { href } from "../lib/router";

interface Hit {
  path: string;
  title: string;
  line: number;
  text: string;
  key: string | null;
}

/** A link to the reference entry of config key `key`. */
export function docLink(key: string): HTMLElement {
  return h("a", { href: href(`/docs?key=${encodeURIComponent(key)}`), "data-doc-key": key }, key);
}

function entry(hit: Hit): HTMLElement {
  if (hit.key) {
    const cells = hit.text.split("|").map((c) => c.trim()).filter((c) => c !== "");
    const [key, type, def, range, applies, description] = cells;
    return h(
      "article",
      { class: "doc-entry", "data-entry": hit.key },
      h("h2", {}, key?.replaceAll("`", "") ?? hit.key),
      h("p", {}, description ?? ""),
      h("dl", {}, h("dt", {}, "Type"), h("dd", {}, type ?? ""), h("dt", {}, "Default"), h("dd", {}, def ?? ""), h("dt", {}, "Values"), h("dd", {}, range ?? ""), h("dt", {}, "Applies"), h("dd", {}, applies ?? "")),
      h("p", { class: "muted" }, `${hit.title} (${hit.path})`),
    );
  }
  return h("li", {}, h("a", { href: href(`/docs?page=${encodeURIComponent(hit.path)}`) }, hit.title || hit.path), `: ${hit.text}`);
}

async function results(query: string): Promise<HTMLElement> {
  const found = await api.get<{ hits: Hit[] }>(`/api/docs/search?q=${encodeURIComponent(query)}`);
  if (found.hits.length === 0) return notice(`Nothing matches ${query}.`);
  const keys = found.hits.filter((hit) => hit.key === query).map(entry);
  const lines = found.hits.filter((hit) => hit.key !== query).map(entry);
  return h("div", { class: "results" }, ...keys, lines.length ? h("ul", {}, ...lines) : null);
}

async function shown(path: string): Promise<HTMLElement> {
  const doc = await api.get<{ title: string; markdown: string }>(`/api/docs/page?path=${encodeURIComponent(path)}`);
  return h("article", { class: "doc-page" }, h("pre", { class: "markdown" }, doc.markdown));
}

export async function docsPage(params: URLSearchParams): Promise<HTMLElement> {
  const output = h("div", { class: "docs-output" });
  const input = h("input", { type: "search", name: "q", "aria-label": "Search the documentation", value: params.get("key") ?? params.get("q") ?? "" });
  const form = h("form", { role: "search" }, input, h("button", { type: "submit" }, "Search"));
  const run = async (query: string) => {
    output.replaceChildren(query.trim() ? await results(query.trim()) : notice("Type a word or a configuration key."));
  };
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    void run(input.value);
  });
  const index = await api.get<{ pages: { path: string; title: string }[] }>("/api/docs");
  const pages = h("ul", { class: "doc-index" }, ...index.pages.map((p) => h("li", {}, h("a", { href: href(`/docs?page=${encodeURIComponent(p.path)}`) }, p.title || p.path))));
  const pageParam = params.get("page");
  if (pageParam) output.replaceChildren(await shown(pageParam));
  else if (input.value) await run(input.value);
  return page("Docs", form, output, h("h2", {}, "Pages"), pages);
}
