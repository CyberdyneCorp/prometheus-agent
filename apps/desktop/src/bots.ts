// The Bots pane (spec: bot-mode): the roster of this connection's bots
// with avatars, previews, unread counts and the active flag, grouped by
// the section stored on each member; hiding, sections, titles; creating,
// duplicating and deleting bots; and each bot's routines. Everything the
// pane shows comes from the daemon (`bot.roster.list`, `bot.meta.*`), so
// a second desktop on the same daemon shows the same bots, and every
// call that changes a bot goes over a connection granted to that bot's
// profile.

import type { BotCreated, BotRoutines, BotSection, RosterRow } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import { avatarPicture, drawAvatar } from "./avatar";
import type { DesktopClient } from "./client";
import { confirmIn } from "./confirm";
import type { ProfileClients } from "./profiles";
import { RoutinesPanel } from "./routines";

/** What the pane does in the window beyond its own requests. */
export interface BotActions {
  /** Opens the bot's canonical Bot Chat in the window. */
  openBotChat: (profile: string) => Promise<void>;
}

const UNASSIGNED = "Unassigned";

/** A section id from its heading: stable for the same name. */
export function sectionId(name: string): string {
  return (
    name
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-|-$/g, "") || "section"
  );
}

/** Rows grouped by section heading, unassigned last. */
export function bySection(rows: RosterRow[]): [BotSection | null, RosterRow[]][] {
  const groups = new Map<string, [BotSection | null, RosterRow[]]>();
  for (const row of rows) {
    const section = row.chat?.section ?? null;
    const key = section?.id ?? "";
    const group = groups.get(key) ?? [section, []];
    group[1].push(row);
    groups.set(key, group);
  }
  const named = [...groups.entries()].filter(([key]) => key !== "").sort(([, a], [, b]) => (a[0]?.name ?? "").localeCompare(b[0]?.name ?? ""));
  const rest = groups.get("");
  return [...named.map(([, group]) => group), ...(rest ? [rest] : [])];
}

export class BotsPane {
  readonly el = h("section", { class: "bots", "aria-label": "Bots" });
  private readonly list = h("div", { class: "bot-roster" });
  private readonly detail = h("div", { class: "bot-detail" });
  private readonly showHidden = h("input", { type: "checkbox", name: "show-hidden", "aria-label": "Show hidden bots" });
  private renders = 0;

  constructor(
    private readonly client: DesktopClient,
    private readonly pool: ProfileClients,
    private readonly dialogs: HTMLElement,
    private readonly actions: BotActions,
  ) {
    this.showHidden.addEventListener("change", () => void this.render());
    const create = h("button", { type: "button", name: "new-bot" }, "New bot");
    create.addEventListener("click", () => this.createForm());
    this.el.append(h("h2", {}, "Bots"), h("div", { class: "bot-tools" }, create, h("label", {}, this.showHidden, " Show hidden")), this.list, this.detail);
  }

  /** Lists the roster again (newest render wins). */
  async render(): Promise<void> {
    const render = ++this.renders;
    delete this.list.dataset.ready;
    let content: HTMLElement[];
    try {
      const roster = await this.client.call<{ rows: RosterRow[] }>("bot.roster.list", { profile: this.client.profile, include_hidden: this.showHidden.checked });
      content = bySection(roster.rows.filter((r) => r.source === "local")).map(([section, rows]) => this.section(section, rows));
    } catch (err) {
      content = [notice((err as Error).message, "error")];
    }
    if (render !== this.renders) return;
    this.list.replaceChildren(...content);
    this.list.dataset.ready = "true";
  }

  /** A call made for `profile` over a connection granted to it. */
  private call<T>(profile: string, method: string, params: Record<string, unknown>): Promise<T> {
    return this.pool.call<T>(profile, method, { profile, ...params });
  }

  private section(section: BotSection | null, rows: RosterRow[]): HTMLElement {
    const heading = h("h3", {}, section?.name ?? UNASSIGNED);
    if (section) {
      const remove = h("button", { type: "button", name: "delete-section", "data-section": section.id }, "Delete section");
      remove.addEventListener("click", () => void this.deleteSection(section));
      heading.append(" ", remove);
    }
    return h("div", { class: "bot-section", "data-section": section?.id ?? "" }, heading, h("ul", {}, ...rows.map((r) => this.row(r))));
  }

  private row(row: RosterRow): HTMLElement {
    const name = row.agent.name;
    const chat = row.chat ?? {};
    const avatar = row.agent.avatar;
    const face = drawAvatar(avatar, name);
    void avatarPicture(avatar, (method, params) => this.call(name, method, params)).then((picture) => {
      if (picture) face.replaceWith(drawAvatar(avatar, name, picture));
    });
    const open = h("button", { type: "button", name: "open-bot", "data-profile": name }, face, h("span", { class: "bot-title" }, row.agent.title ?? name), h("span", { class: "bot-handle" }, ` ${row.handle}`));
    open.addEventListener("click", () => void this.open(name));
    const unread = chat.unread ? h("span", { class: "bot-unread", "aria-label": "Unread messages" }, String(chat.unread)) : null;
    const active = chat.active ? h("span", { class: "bot-active", "aria-label": "Active" }, "●") : null;
    const manage = h("button", { type: "button", name: "manage-bot", "data-profile": name }, "Edit");
    manage.addEventListener("click", () => void this.edit(row));
    return h(
      "li",
      { "data-bot": name, "data-hidden": chat.hidden ? "true" : "false", "data-active": chat.active ? "true" : "false" },
      open,
      unread,
      active,
      h("span", { class: "bot-preview" }, chat.preview ?? ""),
      manage,
    );
  }

  private async open(profile: string): Promise<void> {
    await this.actions.openBotChat(profile);
    await this.call(profile, "bot.chat.read", {}).catch(() => {});
    await this.render();
  }

  private async deleteSection(section: BotSection): Promise<void> {
    await this.client.call("bot.sections.delete", { profile: this.client.profile, section_id: section.id });
    await this.render();
  }

  /** The bot's metadata form, its routines and its lifecycle actions. */
  private async edit(row: RosterRow): Promise<void> {
    const name = row.agent.name;
    const meta = await this.call<{ title?: string | null; description?: string | null; hidden: boolean; section?: BotSection | null }>(name, "bot.meta.get", {});
    const title = h("input", { name: "bot-title", value: meta.title ?? "", "aria-label": "Title" });
    const description = h("input", { name: "bot-description", value: meta.description ?? "", "aria-label": "Description" });
    const section = h("input", { name: "bot-section", value: meta.section?.name ?? "", "aria-label": "Section" });
    const hidden = h("input", { type: "checkbox", name: "bot-hidden", checked: meta.hidden, "aria-label": "Hidden" });
    const upload = h("input", { type: "file", name: "bot-avatar", accept: "image/png,image/jpeg,image/webp", "aria-label": "Avatar image" });
    upload.addEventListener("change", () => {
      const file = upload.files?.[0];
      if (file) void this.guard(this.uploadAvatar(name, file));
    });
    const save = h("form", { class: "bot-meta", "data-profile": name }, title, description, section, h("label", {}, hidden, " Hidden"), h("label", {}, "Avatar ", upload), h("button", { type: "submit", name: "save-bot" }, "Save"));
    save.addEventListener("submit", (event) => {
      event.preventDefault();
      const heading = section.value.trim();
      const changes = { title: title.value.trim() || null, description: description.value.trim() || null, hidden: hidden.checked, section: heading ? { id: sectionId(heading), name: heading } : null };
      void this.guard(this.call(name, "bot.meta.update", { changes }).then(() => this.render()));
    });
    const routines = new RoutinesPanel(name, {
      list: async () => (await this.call<BotRoutines>(name, "bot.routines.list", {})).routines,
      change: async (change, target, body) => void (await this.call(name, "manage.change", { change, target, body })),
    });
    this.detail.replaceChildren(h("h3", {}, row.agent.title ?? name), save, this.lifecycle(name), routines.el);
    this.detail.dataset.profile = name;
    await routines.render();
  }

  /** Sets an uploaded picture as the bot's avatar (kept in its blobs). */
  private async uploadAvatar(name: string, file: File): Promise<void> {
    const bytes = new Uint8Array(await file.arrayBuffer());
    let binary = "";
    for (const byte of bytes) binary += String.fromCharCode(byte);
    const avatar = { kind: "image", mime: file.type, data: btoa(binary) };
    await this.call(name, "bot.meta.update", { changes: { avatar } });
    await this.render();
  }

  /** Duplicate and delete. */
  private lifecycle(name: string): HTMLElement {
    const copyName = h("input", { name: "duplicate-name", placeholder: `${name}-copy`, "aria-label": "Copy's name" });
    const duplicate = h("button", { type: "button", name: "duplicate-bot" }, "Duplicate");
    duplicate.addEventListener("click", () => {
      const copy = copyName.value.trim() || `${name}-copy`;
      void this.guard(this.call<BotCreated>(name, "bot.duplicate", { name: copy }).then(() => this.render()));
    });
    const remove = h("button", { type: "button", name: "delete-bot", disabled: name === "default" }, "Delete");
    remove.addEventListener("click", async () => {
      if (!(await confirmIn(this.dialogs, `Delete bot ${name}? Its profile, sessions and memory are removed.`, "Delete"))) return;
      await this.guard(
        this.call(name, "bot.delete", { confirm: true }).then(() => {
          this.detail.replaceChildren();
          return this.render();
        }),
      );
    });
    return h("div", { class: "bot-lifecycle" }, copyName, duplicate, remove);
  }

  /** The new-bot form: fresh, cloned or empty, optionally on a peer. */
  private createForm(): void {
    const field = (name: string, label: string) => h("input", { name, "aria-label": label, placeholder: label });
    const name = field("bot-name", "Name");
    const title = field("bot-new-title", "Title");
    const description = field("bot-new-description", "Description");
    const cloneFrom = field("bot-clone-from", "Clone from (profile)");
    const model = field("bot-model", "Model (provider/model)");
    const on = field("bot-on", "Create on (peer)");
    const empty = h("input", { type: "checkbox", name: "bot-empty", "aria-label": "No bundled skills" });
    const keys = h("input", { type: "checkbox", name: "bot-copy-keys", checked: true, "aria-label": "Copy API keys" });
    const form = h("form", { class: "bot-create" }, name, title, description, cloneFrom, model, on, h("label", {}, empty, " No bundled skills"), h("label", {}, keys, " Copy API keys"), h("button", { type: "submit", name: "create-bot" }, "Create"));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const text = (input: HTMLInputElement) => input.value.trim() || undefined;
      const params = { profile: this.client.profile, name: name.value.trim(), title: text(title), description: text(description), clone_from: text(cloneFrom), model: text(model), on: text(on), empty: empty.checked, copy_api_keys: keys.checked };
      void this.guard(
        this.client.call<BotCreated>("bot.create", params).then((created) => {
          this.detail.replaceChildren(h("p", { class: "bot-created", "data-profile": created.profile }, `Created ${created.handle} on ${created.device}.`));
          return this.render();
        }),
      );
    });
    this.detail.replaceChildren(h("h3", {}, "New bot"), form);
  }

  /** Shows a failed action in the detail area instead of dropping it. */
  private async guard(work: Promise<unknown>): Promise<void> {
    try {
      await work;
    } catch (err) {
      this.detail.append(notice((err as Error).message, "error"));
    }
  }
}
