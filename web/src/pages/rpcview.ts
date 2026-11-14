// Views read over the client protocol: the harness-evolution runs and the
// experience library (spec: web-dashboard, Harness-evolution view;
// Experience library view). A daemon that does not serve those methods
// yet says so instead of showing an empty list.

import { h, notice, page, table } from "../lib/dom";
import { RpcError, rpc } from "../lib/rpc";
import { profileOf } from "../lib/state";

type Item = Record<string, unknown>;

async function listed(title: string, method: string, profile: string, columns: string[]): Promise<HTMLElement> {
  try {
    const client = await rpc();
    const result = (await client.call(method, { profile })) as Item;
    const items = (Object.values(result).find(Array.isArray) as Item[] | undefined) ?? [];
    return page(
      title,
      items.length ? table(columns, items.map((i) => columns.map((c) => String(i[c] ?? "")))) : notice("Nothing recorded yet."),
    );
  } catch (err) {
    const unserved = err instanceof RpcError && err.failure.data?.code === "rpc.not_served";
    const text = unserved ? `This daemon does not serve ${method} yet, so there is nothing to show.` : (err as Error).message;
    return page(title, notice(text, unserved ? "warn" : "error"));
  }
}

export const evolutionPage = (params: URLSearchParams) =>
  listed("Harness evolution", "evolve.list", profileOf(params), ["id", "mode", "budget", "rollouts", "cost_usd", "status"]);
export const experiencePage = (params: URLSearchParams) =>
  listed("Experience library", "experience.list", profileOf(params), ["domain", "lesson", "source", "state"]);

/** Pages whose capability arrives in a later part of this release. */
export function pendingPage(title: string, what: string): () => HTMLElement {
  return () => page(title, notice(`${what} is not available in this build yet.`, "warn"), h("p", {}, "Use the CLI meanwhile: prometheus --help."));
}
