// What every page shares: the signed-in browser and the profile it works
// on. The profile is always explicit (`?profile=` in links, a selector in
// the header); nothing falls back to a default on the server.

import type { SessionInfo } from "./api";
import type { Prefs } from "./prefs";

export const state = {
  session: null as SessionInfo | null,
  /** The machine's UI preferences, once the protocol connection is up. */
  prefs: null as Prefs | null,
  profiles: [] as string[],
  profile: "",
};

/** The profile a page addresses: `?profile=`, else the selected one. */
export function profileOf(params: URLSearchParams): string {
  const asked = params.get("profile");
  if (asked && state.profiles.includes(asked)) return asked;
  return state.profile;
}

export function canDrive(): boolean {
  return state.session?.role === "driver";
}

let prefsArrived: (prefs: Prefs) => void = () => {};
/** Resolves once the machine's UI preferences are joined. */
export const prefsReady: Promise<Prefs> = new Promise((resolve) => (prefsArrived = resolve));

/** The UI preferences are joined. */
export function prefsStarted(prefs: Prefs): void {
  state.prefs = prefs;
  prefsArrived(prefs);
}
