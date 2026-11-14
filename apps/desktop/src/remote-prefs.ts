// Display preferences stay on this machine (spec: desktop-app, Machine-wide
// UI preferences, Remote connection keeps local preferences): while the
// window shows another connection, `ui.prefs.*` goes to the local daemon
// over a connection of its own, never to the remote daemon.

import type { Envelope } from "../../../schema/rpc";
import type { PrefsChannel } from "../../../web/src/lib/prefs";
import { DesktopClient } from "./client";
import { LOCAL } from "./connections";

export function localPrefsChannel(main: DesktopClient, profile: string): PrefsChannel {
  let own: DesktopClient | null = null;
  const handlers: [string, (envelope: Envelope) => void][] = [];
  const local = (): DesktopClient => {
    if (main.connection === LOCAL) return main;
    if (!own) {
      own = new DesktopClient(profile);
      for (const [event, handler] of handlers) own.on(event, handler);
      own.start();
    }
    return own;
  };
  return {
    call: async (method, params) => {
      const client = local();
      await client.ready();
      return client.call(method, params);
    },
    on: (event, handler) => {
      handlers.push([event, handler]);
      // The main connection's events count only while it is the local one.
      main.on(event, (envelope) => {
        if (main.connection === LOCAL) handler(envelope);
      });
      own?.on(event, handler);
    },
  };
}
