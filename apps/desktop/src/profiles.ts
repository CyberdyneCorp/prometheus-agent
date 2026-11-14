// One protocol connection per profile (spec: desktop-app, Management
// panes; Profile switching is explicit; Gateway switching and scoping): a
// dial ticket grants exactly one profile, so a pane acting on another
// profile (`Applies to`, a job of another profile in the all-profiles
// view) uses a connection granted to that profile, never the window's, on
// the connection the window shows.

import { DesktopClient } from "./client";

export class ProfileClients {
  private readonly others = new Map<string, DesktopClient>();

  constructor(private readonly main: DesktopClient) {}

  /** The connection granted to `profile` (dialed on first use). */
  async of(profile: string): Promise<DesktopClient> {
    if (profile === this.main.profile) return this.main;
    const key = `${this.main.connection}/${profile}`;
    let client = this.others.get(key);
    if (!client) {
      client = new DesktopClient(profile, this.main.connection);
      client.start();
      this.others.set(key, client);
    }
    await client.ready();
    return client;
  }

  /** `method` on `profile`'s connection. */
  async call<T>(profile: string, method: string, params: unknown): Promise<T> {
    return (await this.of(profile)).call<T>(method, params);
  }
}
