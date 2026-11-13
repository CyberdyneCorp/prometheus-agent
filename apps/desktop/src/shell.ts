// The page's only native access (spec: desktop-app, Native command
// allowlist): one typed `shell` command of the Tauri shell, and the events
// the shell sends (parsed deep links). Nothing here touches webview
// storage: a dial ticket lives only in the protocols array handed to the
// WebSocket constructor.

interface TauriGlobal {
  core: { invoke(command: string, args: Record<string, unknown>): Promise<unknown> };
  event: { listen(name: string, handler: (event: { payload: unknown }) => void): Promise<() => void> };
}

declare global {
  interface Window {
    __TAURI__?: TauriGlobal;
  }
}

function tauri(): TauriGlobal {
  const bridge = window.__TAURI__;
  if (!bridge) throw new Error("not running inside the Prometheus desktop app");
  return bridge;
}

/** Runs one allowlisted shell command. */
export function shell<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const request = args === undefined ? { command } : { command, args };
  return tauri().core.invoke("shell", { request }) as Promise<T>;
}

/** Calls `handler` with each payload of shell event `name`. */
export function onShellEvent(name: string, handler: (payload: unknown) => void): void {
  void tauri().event.listen(name, (event) => handler(event.payload));
}

/** A prepared dial: the ticket rides only inside `protocols`. */
export interface Dial {
  dial: string;
  url: string;
  protocols: string[];
}

/** What attaching to the local daemon gave. */
export type Attach =
  | { status: "attached"; version: string; profiles: string[]; notice?: string }
  | { status: "confirm_restart"; running_turns: number; daemon: string; bundled: string }
  | { status: "no_local" }
  | { status: "failed"; error: string; log?: string };

/** The external URL command's answer. */
export interface Opened {
  ok: boolean;
  reason?: string;
}
