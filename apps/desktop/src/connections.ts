// Connections (spec: desktop-app, Connection registry; Remote connections
// are authenticated and encrypted; SSH connections reuse the local ticket
// model; Gateway switching and scoping; Update every connected instance).
// The registry lives in the shell: this page lists it, asks the shell to
// test, trust, pair and remove, and never sees a credential. Sessions left
// open on another connection keep streaming on a background connection of
// their own, whose approvals are answered on that connection.

import type { ApprovalRequestParams, Decision } from "../../../schema/rpc";
import { approvalCard } from "../../../web/src/components/approval";
import { h } from "../../../web/src/lib/dom";
import { DesktopClient, type Latch } from "./client";
import { shell } from "./shell";

export interface SshTarget {
  user?: string;
  host: string;
  port?: number;
  profile?: string;
  binary?: string;
}

export interface ConnectionRow {
  id: string;
  name: string;
  kind: "local" | "remote" | "ssh";
  url?: string;
  ssh?: SshTarget;
}

export interface Listing {
  connections: ConnectionRow[];
  primary: string;
  open_on_launch: "primary" | "last_used";
  launch: string;
  latched: Record<string, Latch>;
  paired: string[];
}

interface Step {
  ok: boolean;
  detail: string;
}

interface TestReport {
  health: Step;
  websocket: Step;
  confirm?: { kind: "certificate" | "host_key"; fingerprint: string };
  latched?: Latch;
}

interface UpdateRow {
  connection_id: string;
  name: string;
  outcome: string;
  message: string;
}

export const LOCAL = "local";

export function listConnections(): Promise<Listing> {
  return shell<Listing>("connections_list");
}

/** Where a connection points, for its row. */
function where(c: ConnectionRow): string {
  if (c.kind === "local") return "this computer";
  if (c.kind === "remote") return c.url ?? "";
  const t = c.ssh;
  return t ? `ssh ${t.user ? `${t.user}@` : ""}${t.host}:${t.port ?? 22}${t.profile ? ` (${t.profile})` : ""}` : "ssh";
}

/** The latched error of a connection, with the action that clears it. */
function latchNotice(c: ConnectionRow, latch: Latch, done: () => void): HTMLElement {
  const fingerprint = latch.kind === "certificate" ? latch.presented : latch.fingerprint;
  const action = h("button", { type: "button", name: "repin" }, latch.kind === "certificate" ? "Re-pin" : "Trust new key");
  action.addEventListener("click", async () => {
    await shell("connection_trust", { id: c.id, fingerprint });
    done();
  });
  return h("div", { class: "latched", role: "alert" }, h("p", {}, latch.message), action);
}

/** The Connections section of the settings. */
export class ConnectionsPanel {
  readonly el = h("section", { class: "connections", "aria-label": "Connections" });
  /** The registry changed (the window refreshes its connection picker). */
  onChange: (listing: Listing) => void = () => {};
  /** A latched error was acted on: the window's client may redial. */
  onRetried: (id: string) => void = () => {};
  private listing: Listing | null = null;

  async render(): Promise<void> {
    const listing = await listConnections();
    this.listing = listing;
    const rows = listing.connections.map((c) => this.row(c, listing));
    this.el.replaceChildren(
      h("h2", {}, "Connections"),
      h("ul", { class: "connection-list" }, ...rows),
      this.onLaunch(listing),
      this.addForm(),
      this.updateAll(),
    );
    this.onChange(listing);
  }

  private row(c: ConnectionRow, listing: Listing): HTMLElement {
    const result = h("div", { class: "test-result" });
    const button = (name: string, label: string, run: () => Promise<unknown>) => {
      const b = h("button", { type: "button", name }, label);
      b.addEventListener("click", () =>
        void run().catch((err: unknown) => result.replaceChildren(h("p", { class: "error", role: "alert" }, String(err)))),
      );
      return b;
    };
    const actions = [button("test", "Test", () => this.test(c, result))];
    if (c.id !== listing.primary) actions.push(button("primary", "Make primary", async () => shell("connection_primary", { id: c.id }).then(() => this.render())));
    if (c.kind !== "local") actions.push(button("remove", "Remove", async () => shell("connection_remove", { id: c.id }).then(() => this.render())));
    const latch = listing.latched[c.id];
    return h(
      "li",
      { "data-connection": c.id },
      h("strong", { class: "name" }, c.name),
      h("span", { class: "where" }, ` ${where(c)}`),
      c.id === listing.primary ? h("span", { class: "badge" }, " primary") : null,
      listing.paired.includes(c.id) ? h("span", { class: "badge" }, " paired") : null,
      latch
        ? latchNotice(c, latch, () => {
            this.onRetried(c.id);
            void this.render();
          })
        : null,
      h("div", { class: "actions" }, ...actions),
      c.kind === "remote" ? this.pairForm(c, result) : null,
      result,
    );
  }

  private async test(c: ConnectionRow, out: HTMLElement): Promise<void> {
    out.replaceChildren(h("p", { role: "status" }, "Testing…"));
    const report = await shell<TestReport>("connection_test", { id: c.id });
    const step = (label: string, s: Step) => h("li", { "data-ok": String(s.ok) }, `${label}: ${s.ok ? "ok" : "failed"} — ${s.detail}`);
    const parts: HTMLElement[] = [h("ul", { class: "steps" }, step("Health", report.health), step("WebSocket", report.websocket))];
    if (report.confirm) parts.push(this.confirm(c, report.confirm, out));
    out.replaceChildren(...parts);
    if (report.latched) void this.render();
  }

  /** First use: the user compares the fingerprint before trusting it. */
  private confirm(c: ConnectionRow, ask: { kind: string; fingerprint: string }, out: HTMLElement): HTMLElement {
    const what =
      ask.kind === "certificate"
        ? "Compare this fingerprint with what `prometheus dashboard --fingerprint` prints on the server:"
        : "Compare this host key fingerprint with the host's (`ssh-keygen -lf` of its host key):";
    const trust = h("button", { type: "button", name: "trust" }, "Trust");
    trust.addEventListener("click", async () => {
      await shell("connection_trust", { id: c.id, fingerprint: ask.fingerprint });
      await this.test(c, out);
    });
    return h("div", { class: "confirm" }, h("p", {}, what), h("code", { class: "fingerprint" }, ask.fingerprint), trust);
  }

  /** A pairing code (or the server's deployment token) goes straight to
   *  the shell, which keeps the credential in the OS keychain. */
  private pairForm(c: ConnectionRow, out: HTMLElement): HTMLElement {
    const kind = h(
      "select",
      { name: "credential-kind", "aria-label": `Credential for ${c.name}` },
      h("option", { value: "code" }, "Pairing code"),
      h("option", { value: "token" }, "Deployment token"),
    );
    const value = h("input", { name: "pairing-code", type: "password", "aria-label": `Pairing code for ${c.name}`, autocomplete: "off" });
    const pair = h("button", { type: "button", name: "pair" }, "Pair");
    pair.addEventListener("click", async () => {
      const entered = value.value.trim();
      value.value = "";
      try {
        await shell("connection_pair", { id: c.id, [kind.value]: entered });
        await this.render();
      } catch (err) {
        out.replaceChildren(h("p", { class: "error", role: "alert" }, String(err)));
      }
    });
    return h("div", { class: "pair" }, kind, value, pair);
  }

  private onLaunch(listing: Listing): HTMLElement {
    const select = h(
      "select",
      { name: "open-on-launch", "aria-label": "Open on launch" },
      h("option", { value: "primary", selected: listing.open_on_launch === "primary" }, "Primary connection"),
      h("option", { value: "last_used", selected: listing.open_on_launch === "last_used" }, "Last used"),
    );
    select.addEventListener("change", () => void shell("connection_on_launch", { mode: select.value }));
    return h("label", {}, "Open on launch ", select);
  }

  private addForm(): HTMLElement {
    const field = (name: string, label: string) => h("input", { name, "aria-label": label, placeholder: label });
    const name = field("connection-name", "Name");
    const kind = h("select", { name: "connection-kind", "aria-label": "Kind" }, h("option", { value: "remote" }, "Remote (https)"), h("option", { value: "ssh" }, "SSH"));
    const url = field("connection-url", "URL");
    const user = field("ssh-user", "SSH user");
    const host = field("ssh-host", "SSH host");
    const port = field("ssh-port", "SSH port");
    const profile = field("ssh-profile", "Remote profile");
    const binary = field("ssh-binary", "Remote binary path");
    const sshFields = h("div", { class: "ssh-fields", hidden: true }, user, host, port, profile, binary);
    kind.addEventListener("change", () => {
      sshFields.hidden = kind.value !== "ssh";
      url.hidden = kind.value === "ssh";
    });
    const error = h("p", { class: "error", role: "alert" });
    const save = h("button", { type: "button", name: "add-connection" }, "Add connection");
    save.addEventListener("click", async () => {
      const opt = (i: HTMLInputElement) => i.value.trim() || undefined;
      const draft =
        kind.value === "ssh"
          ? { name: name.value, kind: "ssh", ssh: { user: opt(user), host: host.value.trim(), port: Number(port.value) || 22, profile: opt(profile), binary: opt(binary) } }
          : { name: name.value, kind: "remote", url: url.value };
      try {
        await shell("connection_save", draft);
        await this.render();
      } catch (err) {
        error.textContent = String(err);
      }
    });
    return h("fieldset", { class: "add-connection" }, h("legend", {}, "Add a connection"), name, kind, url, sshFields, save, error);
  }

  /** Update all instances: each remote updates itself, the app last. */
  private updateAll(): HTMLElement {
    const table = h("table", { class: "update-rows", "aria-label": "Update results" });
    const run = h("button", { type: "button", name: "update-all" }, "Update all instances");
    run.addEventListener("click", async () => {
      run.disabled = true;
      table.replaceChildren(h("tr", {}, h("td", {}, "Updating…")));
      const rows = await shell<UpdateRow[]>("update_all").catch((err: unknown): UpdateRow[] => [
        { connection_id: "", name: "", outcome: "failed", message: String(err) },
      ]);
      table.replaceChildren(
        ...rows.map((r) => h("tr", { "data-connection": r.connection_id, "data-outcome": r.outcome }, h("td", {}, r.name), h("td", {}, r.outcome), h("td", {}, r.message))),
      );
      run.disabled = false;
    });
    const many = (this.listing?.connections.length ?? 1) > 1;
    return h("div", { class: "update-all", hidden: !many }, run, table);
  }

  name(id: string): string {
    return this.listing?.connections.find((c) => c.id === id)?.name ?? id;
  }
}

interface Kept {
  client: DesktopClient;
  name: string;
}

/** Sessions left open on another `(connection, profile)`: each pair keeps
 *  its own connection, so the session keeps streaming and its approvals
 *  are answered on the daemon that owns it. */
export class Background {
  readonly el = h("section", { class: "background-requests", "aria-label": "Other connections" });
  private readonly kept = new Map<string, Kept>();

  constructor(private readonly notify: (id: string, params: ApprovalRequestParams, title: string) => void) {}

  private static key(connection: string, profile: string): string {
    return `${connection}/${profile}`;
  }

  /** Keeps following `sessionId` of `profile` on `connection` (`name`). */
  keep(connection: string, name: string, profile: string, sessionId: string): void {
    const key = Background.key(connection, profile);
    const found = this.kept.get(key);
    if (found) {
      found.client.watch(sessionId, null, null);
      void found.client.call("session.attach", { session_id: sessionId }).catch(() => {});
      return;
    }
    const client = new DesktopClient(profile, connection);
    client.watch(sessionId, null, null);
    client.onRequest = (request) => {
      if (request.method !== "approval.request") return;
      const params = request.params as unknown as ApprovalRequestParams;
      this.card(client, name, profile, request.id, params);
      this.notify(request.id, params, `${name}: ${params.tool}`);
    };
    for (const event of ["request.resolved", "request.cancel"]) {
      client.on(event, (envelope) => this.settled((envelope.data as { id: string }).id));
    }
    this.kept.set(key, { client, name });
    client.start();
  }

  /** The window shows `(connection, profile)` again: its own connection
   *  takes the sessions back. */
  release(connection: string, profile: string): void {
    const key = Background.key(connection, profile);
    this.kept.get(key)?.client.close();
    this.kept.delete(key);
    for (const card of this.el.querySelectorAll<HTMLElement>(`[data-pair="${CSS.escape(key)}"]`)) card.remove();
  }

  private card(client: DesktopClient, name: string, profile: string, id: string, params: ApprovalRequestParams): void {
    const answer = async (decision: Decision) => {
      await client.call("approval.respond", { id, decision });
    };
    this.el.append(
      h(
        "div",
        { class: "background-request", "data-request": id, "data-pair": Background.key(client.connection, profile), "data-connection": name },
        h("p", { class: "origin" }, `${name} · ${profile}`),
        approvalCard(id, params, answer, true),
      ),
    );
  }

  private settled(id: string): void {
    const card = this.el.querySelector<HTMLElement>(`[data-request="${CSS.escape(id)}"]`);
    if (card && card.querySelector(".card-status")?.textContent === "") card.remove();
  }
}

/** Local display state of the sidebar's connection and profile groups:
 *  names and order the user chose. It never reaches the registry or any
 *  daemon. */
export class Groups {
  private static readonly KEY = "prometheus.connection-groups";
  private state: { order: string[]; labels: Record<string, string> } = { order: [], labels: {} };

  constructor() {
    try {
      const raw = localStorage.getItem(Groups.KEY);
      if (raw) this.state = JSON.parse(raw) as typeof this.state;
    } catch {
      // Defaults: registry order and names.
    }
  }

  private save(): void {
    try {
      localStorage.setItem(Groups.KEY, JSON.stringify(this.state));
    } catch {
      // Display state only.
    }
  }

  label(key: string, fallback: string): string {
    return this.state.labels[key] ?? fallback;
  }

  rename(key: string, label: string): void {
    if (label.trim()) this.state.labels[key] = label.trim();
    else delete this.state.labels[key];
    this.save();
  }

  /** `keys` in the user's order (unknown ones keep theirs, last). */
  sorted(keys: string[]): string[] {
    const rank = (k: string) => {
      const i = this.state.order.indexOf(k);
      return i < 0 ? Number.MAX_SAFE_INTEGER : i;
    };
    return [...keys].sort((a, b) => rank(a) - rank(b));
  }

  move(keys: string[], key: string, by: -1 | 1): void {
    const order = this.sorted(keys);
    const at = order.indexOf(key);
    const to = at + by;
    if (at < 0 || to < 0 || to >= order.length) return;
    [order[at], order[to]] = [order[to] as string, order[at] as string];
    this.state.order = order;
    this.save();
  }
}
