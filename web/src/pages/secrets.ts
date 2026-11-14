// `/secrets`: names, sources and change times; set and delete. Values are
// write-only: no answer of the daemon carries one (spec: web-dashboard,
// Secrets are write-only).

import { api, profilePath } from "../lib/api";
import { clear, h, notice, page, table } from "../lib/dom";
import { canDrive, profileOf } from "../lib/state";

interface Secret {
  name: string;
  source: string;
  shadowed: boolean;
  modified: string | null;
}

export async function secretsPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const list = h("div");
  const result = h("div");
  const load = async () => {
    const data = await api.get<{ secrets: Secret[] }>(profilePath(profile, "/secrets"));
    clear(list);
    list.append(
      data.secrets.length
        ? table(
            ["Name", "Source", "Changed", ""],
            data.secrets.map((s) => [
              h("code", { "data-secret": s.name }, s.name),
              `${s.source}${s.shadowed ? " (shadowed)" : ""}`,
              s.modified ?? "",
              canDrive() && s.source === "file" ? h("button", { type: "button", class: "danger", onclick: () => void remove(s.name) }, "Delete") : "",
            ]),
          )
        : notice("No secrets."),
    );
  };
  const remove = async (name: string) => {
    if (!window.confirm(`Delete secret ${name}?`)) return;
    await api.del(profilePath(profile, `/secrets/${encodeURIComponent(name)}`));
    await load();
  };
  const name = h("input", { name: "name", placeholder: "NAME", pattern: "[A-Z][A-Z0-9_]*", required: true, "aria-label": "Secret name" });
  const value = h("input", { name: "value", type: "password", autocomplete: "off", required: true, "aria-label": "Secret value" });
  const form = h("form", { class: "toolbar" }, name, value, h("button", { type: "submit", disabled: !canDrive() }, "Set"));
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      await api.put(profilePath(profile, `/secrets/${encodeURIComponent(name.value)}`), { value: value.value });
      value.value = "";
      result.replaceChildren(notice(`Saved ${name.value}.`));
      await load();
    } catch (err) {
      result.replaceChildren(notice((err as Error).message, "error"));
    }
  });
  await load();
  return page("Secrets", h("p", { class: "muted" }, "Values are write-only: they are never shown again."), form, result, list);
}
