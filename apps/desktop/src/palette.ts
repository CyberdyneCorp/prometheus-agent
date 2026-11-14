// The slash palette (spec: desktop-app, Slash palette from one command
// registry): populated from `commands.catalog`, whose per-surface and
// per-subcommand dispositions the daemon's registry authors once. A
// command is offered when it or one of its subcommands runs here;
// extension commands (skills, plugin and quick commands) always are; an
// unavailable command shows the registry's reason verbatim.

export interface PaletteSub {
  name: string;
  disposition: "exec" | "unavailable" | "hidden";
  reason?: string | null;
}

export interface PaletteEntry {
  name: string;
  aliases?: string[];
  description: string;
  source: string;
  disposition: "exec" | "unavailable" | "hidden";
  reason?: string | null;
  subcommands?: PaletteSub[];
}

const runs = (d: string) => d === "exec";
const extension = (e: PaletteEntry) => e.source !== "builtin" && e.source !== "client";

function named(entry: PaletteEntry, word: string): boolean {
  return entry.name === word || (entry.aliases ?? []).includes(word);
}

function offered(entry: PaletteEntry): boolean {
  if (entry.disposition === "hidden") return false;
  return extension(entry) || runs(entry.disposition) || (entry.subcommands ?? []).some((s) => runs(s.disposition));
}

/** Completions for what is typed (`/sk`, `/skills p`). */
export function suggest(catalog: PaletteEntry[], typed: string): string[] {
  const text = typed.replace(/^\//, "");
  const space = text.indexOf(" ");
  if (space < 0) {
    return catalog
      .filter((e) => offered(e) && (e.name.startsWith(text) || (e.aliases ?? []).some((a) => a.startsWith(text))))
      .map((e) => `/${e.name}`);
  }
  const head = text.slice(0, space);
  const rest = text.slice(space + 1).trimStart();
  if (rest.includes(" ")) return [];
  const entry = catalog.find((e) => named(e, head));
  if (!entry) return [];
  return (entry.subcommands ?? [])
    .filter((s) => runs(s.disposition) && s.name.startsWith(rest))
    .map((s) => `/${entry.name} ${s.name}`);
}

/** Why `/line` cannot run here, verbatim from the registry; null when it
 *  runs (or is no command at all, and goes to the agent as a prompt). */
export function refusal(catalog: PaletteEntry[], line: string): string | null {
  const [head = "", sub = ""] = line.replace(/^\//, "").split(/\s+/);
  const entry = catalog.find((e) => named(e, head));
  if (!entry || extension(entry)) return null;
  const subEntry = (entry.subcommands ?? []).find((s) => s.name === sub);
  if (subEntry) return runs(subEntry.disposition) ? null : (subEntry.reason ?? entry.reason ?? null);
  return runs(entry.disposition) ? null : (entry.reason ?? `/${entry.name} is not available here.`);
}
