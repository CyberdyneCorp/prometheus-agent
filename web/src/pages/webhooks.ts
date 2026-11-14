// `/webhooks` (spec: web-dashboard, Webhooks page): the profile's webhook
// subscriptions with URL, event filter, delivery target and enabled
// state; create one (its route URL and HMAC secret are shown once, right
// after creation, and no API returns the secret again), enable or disable
// it (a disabled route answers 403, no restart), delete it and send a
// signed test event.

import { api, ApiError, profilePath } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { href } from "../lib/router";
import { canDrive, profileOf } from "../lib/state";

interface Subscription {
  name: string;
  description: string;
  url: string;
  events: string[];
  deliver: string;
  enabled: boolean;
}

interface Listing {
  subscriptions: Subscription[];
  adapter_enabled: boolean;
}

interface Created {
  name: string;
  url: string;
  secret: string | null;
}

function field(label: string, name: string, attrs: Record<string, string | boolean> = {}): HTMLElement {
  return h("label", {}, label, " ", h("input", { name, ...attrs }));
}

function createForm(profile: string, done: (created: Created) => void, status: HTMLElement): HTMLFormElement {
  const form = h(
    "form",
    { "aria-label": "New webhook" },
    field("Name", "name", { required: true }),
    field("Description", "description"),
    field("Events (comma-separated)", "events"),
    field("Delivery target", "deliver", { placeholder: "origin, log or platform:chat" }),
    h("label", {}, h("input", { type: "checkbox", name: "deliver_only" }), " Deliver directly (no model)"),
    h("label", {}, "Agent prompt ", h("textarea", { name: "prompt", rows: 2 })),
    h("button", { type: "submit", disabled: !canDrive() }, "Create"),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const data = new FormData(form);
    try {
      const created = await api.post<Created>(profilePath(profile, "/webhooks"), {
        name: data.get("name"),
        description: data.get("description"),
        events: data.get("events"),
        deliver: data.get("deliver"),
        deliver_only: data.get("deliver_only") === "on",
        prompt: data.get("prompt"),
      });
      done(created);
    } catch (err) {
      status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    }
  });
  return form;
}

function secretOnce(created: Created): HTMLElement {
  return h(
    "div",
    { class: "notice warn", role: "status", "data-created": created.name },
    h("p", {}, `Created ${created.name}. Copy the secret now: it is shown only this once.`),
    h("p", {}, "URL: ", h("code", { "data-url": "" }, created.url)),
    created.secret ? h("p", {}, "Secret: ", h("code", { "data-secret": "" }, created.secret)) : null,
  );
}

function actions(profile: string, sub: Subscription, reload: () => void, status: HTMLElement): HTMLElement {
  const run = (work: () => Promise<unknown>, said: string) => async () => {
    try {
      await work();
      status.replaceChildren(notice(said));
      reload();
    } catch (err) {
      status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    }
  };
  const path = profilePath(profile, `/webhooks/${encodeURIComponent(sub.name)}`);
  const button = (label: string, work: () => Promise<unknown>, said: string) => {
    const b = h("button", { type: "button", disabled: !canDrive() }, label);
    b.addEventListener("click", run(work, said));
    return b;
  };
  return h(
    "span",
    { class: "actions" },
    button(sub.enabled ? "Disable" : "Enable", () => api.patch(path, { enabled: !sub.enabled }), `${sub.name} ${sub.enabled ? "disabled" : "enabled"}`),
    button("Test", () => api.post(`${path}/test`), `Sent a signed test event to ${sub.name}`),
    button("Delete", async () => {
      if (window.confirm(`Delete ${sub.name}?`)) await api.del(path);
    }, `Deleted ${sub.name}`),
  );
}

export async function webhooksPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const status = h("div", { class: "status" });
  const list = h("div", {});
  const reload = async () => {
    try {
      const data = await api.get<Listing>(profilePath(profile, "/webhooks"));
      const off = data.adapter_enabled
        ? null
        : h("p", { class: "notice warn", "data-adapter": "off" }, "The webhook adapter is disabled for this profile: ", h("a", { href: href("/adapters") }, "enable it on Adapters"), ".");
      const rows = data.subscriptions.map((s) => [s.name, s.description, s.url, s.events.join(",") || "*", s.deliver, s.enabled ? "enabled" : "disabled", actions(profile, s, () => void reload(), status)]);
      list.replaceChildren(
        ...(off ? [off] : []),
        rows.length ? table(["Name", "Description", "URL", "Events", "Delivery", "State", ""], rows) : notice("No webhook subscriptions yet."),
      );
    } catch (err) {
      list.replaceChildren(notice((err as Error).message, "error"));
    }
  };
  await reload();
  const form = createForm(profile, (created) => {
    status.replaceChildren(secretOnce(created));
    form.reset();
    void reload();
  }, status);
  return page("Webhooks", h("p", { class: "muted" }, `Profile ${profile}`), status, list, h("h2", {}, "New subscription"), form);
}
