// Background running (spec: desktop-app, Approval notifications never
// approve; Closing windows does not stop the agent). An approval for a
// window that is not focused becomes an OS notification naming the
// session and the tool; it has no actions, so nothing but the card in the
// window answers it. Clicking it activates the app, and the shell hands
// this window the request to focus. The tray lists the pending approvals
// and the running sessions.

import type { ApprovalRequestParams } from "../../../schema/rpc";
import { onShellEvent, shell } from "./shell";

interface Line {
  session_id: string;
  label: string;
}

export class Attention {
  private focused = document.hasFocus();
  private readonly approvals = new Map<string, Line>();
  private readonly running = new Map<string, Line>();

  constructor() {
    onShellEvent("window-focus", (payload) => {
      this.focused = (payload as { focused: boolean }).focused;
    });
    window.addEventListener("focus", () => (this.focused = true));
    window.addEventListener("blur", () => (this.focused = false));
  }

  /** An approval request reached the window. */
  approval(requestId: string, params: ApprovalRequestParams, title: string | null): void {
    const session = title ?? params.session_id;
    this.approvals.set(requestId, { session_id: params.session_id, label: `${session}: ${params.tool}` });
    if (!this.focused || document.hidden) {
      void shell("notify_approval", { request_id: requestId, session_id: params.session_id, session_title: title, tool: params.tool }).catch((err: unknown) =>
        console.warn("approval notification failed:", err),
      );
    }
    this.tray();
  }

  /** A request was answered here or elsewhere, or settled by its deadline. */
  settled(requestId: string): void {
    if (!this.approvals.delete(requestId)) return;
    void shell("request_settled", { request_id: requestId }).catch(() => {});
    this.tray();
  }

  /** A turn of `sessionId` started or ended. */
  turn(sessionId: string, running: boolean, label: string): void {
    if (running) this.running.set(sessionId, { session_id: sessionId, label });
    else this.running.delete(sessionId);
    this.tray();
  }

  private tray(): void {
    void shell("set_tray", { approvals: [...this.approvals.values()], running: [...this.running.values()] }).catch(() => {});
  }
}

/** Scrolls to and focuses the card of `requestId`; never answers it. */
export function focusCard(requestId: string): boolean {
  const card = document.querySelector<HTMLElement>(`article[data-request="${CSS.escape(requestId)}"]`);
  if (!card) return false;
  card.tabIndex = -1;
  card.dataset.focused = "true";
  card.scrollIntoView({ block: "center" });
  card.focus();
  return true;
}
