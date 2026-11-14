// Navigation by connection, then profile (spec: desktop-app, Gateway
// switching and scoping): the header's connection picker and the
// sidebar's groups, which the user may rename and reorder. Names and order
// are local display state ({@link Groups}); the registry keeps its own.

import { h } from "../../../web/src/lib/dom";
import type { Groups, Listing } from "./connections";

export class Switcher {
  readonly picker = h("select", { name: "connection", "aria-label": "Connection" });
  readonly el = h("nav", { class: "connection-groups", "aria-label": "Connections and profiles" });
  private listing: Listing | null = null;
  private current = "local";
  private profiles: string[] = [];
  private profile = "default";

  constructor(
    private readonly groups: Groups,
    private readonly go: (connection: string, profile: string | null) => void,
  ) {
    this.picker.addEventListener("change", () => this.go(this.picker.value, null));
  }

  /** The registry, the connection shown and its profiles. */
  update(listing: Listing | null, current: string, profiles: string[], profile: string): void {
    if (listing) this.listing = listing;
    this.current = current;
    this.profiles = profiles;
    this.profile = profile;
    this.render();
  }

  /** What the sidebar calls profile `profile` of `connection`. */
  profileLabel(connection: string, profile: string): string {
    return this.groups.label(`p:${connection}/${profile}`, profile);
  }

  private render(): void {
    const connections = this.listing?.connections ?? [];
    const keys = this.groups.sorted(connections.map((c) => `c:${c.id}`));
    const byKey = new Map(connections.map((c) => [`c:${c.id}`, c]));
    this.picker.replaceChildren(
      ...keys.map((key) => {
        const c = byKey.get(key);
        return h("option", { value: c?.id ?? "", selected: c?.id === this.current }, this.groups.label(key, c?.name ?? ""));
      }),
    );
    this.el.replaceChildren(
      h(
        "ul",
        {},
        ...keys.map((key) => {
          const c = byKey.get(key);
          if (!c) return null;
          const nested = c.id === this.current ? this.profileGroups(c.id) : null;
          return h("li", { "data-group": key }, this.row(key, c.name, keys, () => this.go(c.id, null)), nested);
        }),
      ),
    );
  }

  private profileGroups(connection: string): HTMLElement {
    const keys = this.groups.sorted(this.profiles.map((p) => `p:${connection}/${p}`));
    return h(
      "ul",
      { class: "profile-groups" },
      ...keys.map((key) => {
        const profile = key.slice(key.indexOf("/") + 1);
        return h("li", { "data-group": key, "data-current": String(profile === this.profile) }, this.row(key, profile, keys, () => this.go(connection, profile)));
      }),
    );
  }

  /** A group's label, its open action, rename and move up/down. */
  private row(key: string, fallback: string, siblings: string[], open: () => void): HTMLElement {
    const label = h("button", { type: "button", class: "group-label" }, this.groups.label(key, fallback));
    label.addEventListener("click", open);
    const rename = h("button", { type: "button", name: "rename-group", "aria-label": `Rename ${fallback}` }, "✎");
    rename.addEventListener("click", () => {
      const input = h("input", { name: "group-name", "aria-label": `New name for ${fallback}`, value: this.groups.label(key, fallback) });
      input.addEventListener("keydown", (e) => {
        if (e.key === "Escape") this.render();
        if (e.key !== "Enter") return;
        this.groups.rename(key, input.value === fallback ? "" : input.value);
        this.render();
      });
      label.replaceWith(input);
      input.focus();
    });
    const move = (by: -1 | 1, text: string, name: string) => {
      const b = h("button", { type: "button", name, "aria-label": `${name === "move-up" ? "Move up" : "Move down"} ${fallback}` }, text);
      b.addEventListener("click", () => {
        this.groups.move(siblings, key, by);
        this.render();
      });
      return b;
    };
    return h("span", { class: "group" }, label, rename, move(-1, "▲", "move-up"), move(1, "▼", "move-down"));
  }
}
