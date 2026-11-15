// The desktop relay courier (spec: agent-interop, Desktop relay for
// cross-connection bot messages). A desktop that holds several
// connections carries bot messages between their daemons, which never
// receive each other's credentials: it drains each profile's relay outbox
// (`bot.relay.drain`, reporting the agents of its other connections),
// hands each envelope to the target daemon over that connection's own
// socket (`bot.relay.deliver`) and carries the reply back the same way.
// An envelope it cannot deliver is simply left: the sending daemon offers
// it again after its delivery deadline.

import type {
  RelayConnection,
  RelayDelivered,
  RelayDrained,
  RelayEnvelope,
  Roster,
} from "../../../schema/rpc";
import { DesktopClient } from "./client";
import { listConnections } from "./connections";

/** One `(connection, profile)` the courier talks to. */
export interface CourierLink {
  /** The connection's label (`<handle>@<label>` targets name it). */
  label: string;
  profile: string;
  call<T>(method: string, params: unknown): Promise<T>;
}

/** Where an envelope goes: the target profile and connection label. */
export function targetOf(envelope: RelayEnvelope): { profile: string; label: string } | null {
  const at = envelope.target.lastIndexOf("@");
  if (at <= 0) return null;
  return { profile: envelope.target.slice(0, at).replace(/^@/, ""), label: envelope.target.slice(at + 1) };
}

/** Carries every drainable envelope of `links` once. */
export async function carryAll(links: CourierLink[]): Promise<void> {
  const byLabel = new Map<string, RelayConnection>();
  for (const link of links) {
    const roster = await link.call<Roster>("bot.roster.list", { profile: link.profile }).catch(() => null);
    if (!roster) continue;
    const known = byLabel.get(link.label) ?? { label: link.label, agents: [] };
    for (const row of roster.rows) {
      if (row.source === "local" && !known.agents.some((a) => a.name === row.agent.name)) known.agents.push(row.agent);
    }
    byLabel.set(link.label, known);
  }
  for (const from of links) {
    const connections = [...byLabel.values()].filter((c) => c.label !== from.label);
    const drained = await from
      .call<RelayDrained>("bot.relay.drain", { profile: from.profile, connections })
      .catch(() => null);
    for (const envelope of drained?.envelopes ?? []) await carry(from, envelope, links);
  }
}

async function carry(from: CourierLink, envelope: RelayEnvelope, links: CourierLink[]): Promise<void> {
  const target = targetOf(envelope);
  if (!target) return;
  const to =
    links.find((l) => l.label === target.label && l.profile === target.profile) ??
    links.find((l) => l.label === target.label);
  if (!to) return;
  const delivered = await to
    .call<RelayDelivered>("bot.relay.deliver", { profile: target.profile, envelope })
    .catch(() => null);
  if (!delivered || delivered.outcome === "running") return;
  const reply = {
    delivery_id: envelope.delivery_id,
    signature: envelope.signature,
    reply: delivered.reply ?? null,
    reason: delivered.reason ?? null,
  };
  await from.call<RelayDelivered>("bot.relay.deliver", { profile: from.profile, reply }).catch(() => null);
}

/** How often the courier looks for envelopes. */
const EVERY_MS = 15_000;

/** The running courier: one background connection per registered
 *  connection and profile it learned of, kept between rounds. */
export class Courier {
  private readonly clients = new Map<string, DesktopClient>();
  private timer: number | undefined;
  private busy = false;

  start(): void {
    this.timer = window.setInterval(() => void this.round(), EVERY_MS);
  }

  stop(): void {
    window.clearInterval(this.timer);
    for (const client of this.clients.values()) client.close();
    this.clients.clear();
  }

  private client(connection: string, profile: string): DesktopClient {
    const key = `${connection}/${profile}`;
    let client = this.clients.get(key);
    if (!client) {
      client = new DesktopClient(profile, connection);
      client.start();
      this.clients.set(key, client);
    }
    return client;
  }

  private async round(): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    try {
      const listing = await listConnections().catch(() => null);
      // A single connection has nobody to relay to.
      if (!listing || listing.connections.length < 2) return;
      const links: CourierLink[] = [];
      for (const connection of listing.connections) {
        const primary = this.client(connection.id, "default");
        const roster = await primary.call<Roster>("bot.roster.list", { profile: "default" }).catch(() => null);
        const profiles = roster?.rows.filter((r) => r.source === "local").map((r) => r.agent.name) ?? ["default"];
        for (const profile of profiles) {
          const client = this.client(connection.id, profile);
          links.push({ label: connection.name, profile, call: (method, params) => client.call(method, params) });
        }
      }
      await carryAll(links);
    } finally {
      this.busy = false;
    }
  }
}
