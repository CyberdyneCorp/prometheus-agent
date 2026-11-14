// The gone-session verdict (spec: desktop-app, Gone-session verdict): a
// failed resume becomes a fresh draft only when the daemon answered
// `-32003` (not found) for the session and nothing else could explain the
// failure; otherwise the window stays on the session and offers Retry.

import { RpcError } from "../../../web/src/lib/rpc";

/** `-32003`: the session does not exist. */
export const NOT_FOUND = -32003;

export interface InFlight {
  reconnecting: boolean;
  switchingProfile: boolean;
}

export function resumeVerdict(error: unknown, state: InFlight): "draft" | "retry" {
  const gone = error instanceof RpcError && error.code === NOT_FOUND;
  return gone && !state.reconnecting && !state.switchingProfile ? "draft" : "retry";
}
