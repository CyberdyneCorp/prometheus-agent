// The quick-ask panel (spec: desktop-app, Global hotkey quick-ask): a
// floating window the global hotkey shows over the current app. A question
// goes to the quick-ask session of `desktop.quick_ask.profile` (else the
// last profile used in the main window), reusing the previous quick-ask
// session when it was active within `desktop.quick_ask.reuse_secs`. Esc
// hides the panel, Ctrl/Cmd+Enter opens the session in the main window,
// and the clipboard is attached only when the attach control is pressed.

import { Transcript } from "../../../web/src/components/transcript";
import { h, notice } from "../../../web/src/lib/dom";
import { DesktopClient } from "./client";
import { onShellEvent, shell } from "./shell";

interface Options {
  quick_ask: { profile: string | null };
}

/** The last profile used in the main window (a per-machine convenience). */
export const LAST_PROFILE = "prometheus.lastProfile";

function lastProfile(): string {
  try {
    return localStorage.getItem(LAST_PROFILE) ?? "default";
  } catch {
    return "default";
  }
}

export async function quickAsk(root: HTMLElement): Promise<void> {
  const attached = await shell<{ status: string; profiles?: string[] }>("attach", {}).catch(() => ({ status: "failed" }));
  if (attached.status !== "attached") {
    root.replaceChildren(notice("Prometheus is not running; open the main window.", "warn"));
    return;
  }
  const options = await shell<Options>("launch_options");
  const profile = options.quick_ask.profile ?? lastProfile();
  const client = new DesktopClient(profile);
  const transcript = new Transcript();
  const input = h("textarea", { name: "quick", rows: 2, placeholder: "Ask Prometheus", "aria-label": "Quick question" });
  const attach = h("button", { type: "button", name: "attach" }, "Attach clipboard");
  const problems = h("div", { class: "problems" });
  let sessionId: string | null = null;
  let clip: string | null = null;

  attach.addEventListener("click", async () => {
    // Only this explicit action reads the clipboard.
    try {
      clip = (await shell<{ text: string }>("read_clipboard")).text;
      attach.textContent = "Clipboard attached";
    } catch (err) {
      problems.replaceChildren(notice(String(err), "warn"));
    }
  });
  const ask = async () => {
    const question = input.value.trim();
    if (!question) return;
    const text = clip ? `${question}\n\n---\n${clip}` : question;
    const target = await shell<{ session_id: string | null }>("quick_ask_target", { profile });
    sessionId = target.session_id;
    if (!sessionId) {
      sessionId = (await client.call<{ session_id: string }>("session.create", { profile, source: "desktop" })).session_id;
    }
    await client.call("session.attach", { session_id: sessionId });
    client.watch(sessionId, null, null);
    transcript.message("user", question);
    input.value = "";
    clip = null;
    attach.textContent = "Attach clipboard";
    await client.call("prompt.submit", { session_id: sessionId, text, files: [] });
    await shell("quick_ask_used", { profile, session_id: sessionId });
  };
  input.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      void shell("hide_panel");
    } else if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      if (sessionId) void shell("open_in_main", { profile, session_id: sessionId });
    } else if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      ask().catch((err: unknown) => problems.replaceChildren(notice((err as Error).message, "error")));
    }
  });
  client.onEvent = (envelope) => {
    if (envelope.session_id && envelope.session_id === sessionId) transcript.apply(envelope);
  };
  onShellEvent("quick-ask-shown", () => input.focus());
  root.replaceChildren(h("section", { class: "quick-ask", "aria-label": "Quick ask" }, input, attach, problems, transcript.el));
  client.start();
  input.focus();
}
