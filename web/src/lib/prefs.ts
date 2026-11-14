// Machine-wide UI preferences (spec: desktop-app, Machine-wide UI
// preferences), shared by the dashboard, the webapp and every desktop
// window: the local daemon stores them (`ui.prefs.get`/`ui.prefs.patch`)
// and tells every watching connection about each change
// (`ui.prefs.changed`), so a theme switched in one tab repaints the others
// at once. Only display preferences go there; a client meeting the store
// for the first time adds what it has without overwriting anything.

import type { Envelope } from "../../../schema/rpc";

export type PrefMap = Record<string, unknown>;

interface Stored {
  prefs: PrefMap;
  revision: number;
}

/** What the preferences need of a protocol connection. */
export interface PrefsChannel {
  call(method: string, params: unknown): Promise<unknown>;
  on(event: string, handler: (envelope: Envelope) => void): void;
}

/** `system` follows the OS; `light` and `dark` force a mode. */
export const MODES = ["system", "light", "dark"] as const;

/** Applies the light/dark mode to the document. */
export function applyMode(mode: unknown): void {
  const root = document.documentElement;
  if (mode === "light" || mode === "dark") root.dataset.mode = mode;
  else delete root.dataset.mode;
}

export class Prefs {
  values: PrefMap = {};
  private revision = -1;
  private watching = false;
  private readonly listeners = new Set<(values: PrefMap) => void>();

  constructor(private readonly channel: PrefsChannel) {}

  /** Reads the store and watches it; `local` holds what this client used
   *  so far, added only where the store has nothing. Called again after a
   *  reconnect (a new connection watches again; a store rebuilt after
   *  corruption gets this client's values back). */
  async start(local: PrefMap = {}): Promise<void> {
    if (!this.watching) {
      this.channel.on("ui.prefs.changed", (envelope) => this.apply(envelope.data as unknown as Stored));
      this.watching = true;
    }
    // A rebuilt store starts again at revision 0.
    this.revision = -1;
    this.apply((await this.channel.call("ui.prefs.get", { watch: true })) as Stored);
    const missing = Object.fromEntries(Object.entries(local).filter(([key, value]) => value !== undefined && !(key in this.values)));
    if (Object.keys(missing).length > 0) {
      this.apply((await this.channel.call("ui.prefs.patch", { set: missing, only_missing: true })) as Stored);
    }
  }

  /** Stores one preference for the whole machine. */
  async set(key: string, value: unknown): Promise<void> {
    this.apply((await this.channel.call("ui.prefs.patch", { set: { [key]: value } })) as Stored);
  }

  /** Calls `handler` now and after every change. */
  onChange(handler: (values: PrefMap) => void): void {
    this.listeners.add(handler);
    handler(this.values);
  }

  private apply(stored: Stored): void {
    if (stored.revision < this.revision) return;
    this.revision = stored.revision;
    this.values = stored.prefs;
    applyMode(this.values.mode);
    for (const handler of this.listeners) handler(this.values);
  }
}
