// What every page shares: the signed-in browser and the profile it works
// on. The profile is always explicit (`?profile=` in links, a selector in
// the header); nothing falls back to a default on the server.

import type { SessionInfo } from "./api";

export const state = {
  session: null as SessionInfo | null,
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
