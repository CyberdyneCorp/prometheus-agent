// `/chat`: one session of the addressed profile, rendered from the client
// protocol (spec: web-dashboard, Structured chat over the client
// protocol). No PTY or shell is involved: the browser is a `dashboard`
// client of the daemon like any other.

import type { ApprovalRequestParams, ClarifyRequestParams, Decision } from "../../../schema/rpc";
import { approvalCard } from "../components/approval";
import { clarifyCard } from "../components/clarify";
import { Transcript } from "../components/transcript";
import { api, profilePath } from "../lib/api";
import { h, notice, page } from "../lib/dom";
import { type Rpc, type ServerRequest, rpc } from "../lib/rpc";
import { canDrive, profileOf } from "../lib/state";

interface Uploaded {
  path: string;
}

function serverRequests(client: Rpc, transcript: Transcript, current: () => string | null): void {
  client.onRequest = (request: ServerRequest) => {
    if (request.params.session_id !== current()) return;
    if (request.method === "approval.request") {
      const answer = async (decision: Decision) => {
        await client.request("approval.respond", { id: request.id, decision });
      };
      transcript.card(request.id, approvalCard(request.id, request.params as unknown as ApprovalRequestParams, answer, canDrive()));
    } else if (request.method === "clarify.request") {
      const answer = (answers: string[]) => client.answer(request.id, { answers });
      transcript.card(request.id, clarifyCard(request.params as unknown as ClarifyRequestParams, answer, canDrive()));
    }
  };
}

export async function chatPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  let session = params.get("session");
  const transcript = new Transcript();
  const problem = h("div", { class: "problems" });
  const client = await rpc();
  client.onEvent = (envelope) => {
    if (envelope.session_id && envelope.session_id === session) transcript.apply(envelope);
  };
  serverRequests(client, transcript, () => session);
  if (session) {
    await client.request("session.attach", { session_id: session, role: canDrive() ? "driver" : "observer" });
    const stored = await client.request("session.transcript", { session_id: session });
    transcript.load(stored.messages);
  }
  const files: string[] = [];
  const input = h("textarea", { name: "prompt", rows: 3, placeholder: "Message the agent", disabled: !canDrive(), "aria-label": "Message" });
  const attach = h("input", { type: "file", disabled: !canDrive(), "aria-label": "Attach a file" });
  const attached = h("span", { class: "muted" });
  attach.addEventListener("change", async () => {
    const file = attach.files?.[0];
    if (!file) return;
    try {
      const uploaded = await api.upload<Uploaded>(profilePath(profile, "/uploads"), file);
      files.push(uploaded.path);
      attached.textContent = `Attached ${file.name}`;
    } catch (err) {
      problem.replaceChildren(notice(`Upload refused: ${(err as Error).message}`, "error"));
    }
    attach.value = "";
  });
  const send = h("button", { type: "submit", disabled: !canDrive() }, "Send");
  const form = h("form", { class: "composer" }, input, h("div", { class: "row" }, attach, attached, send));
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    problem.replaceChildren();
    try {
      if (!session) {
        session = (await client.request("session.create", { profile })).session_id;
        history.replaceState(null, "", `${location.pathname}?profile=${encodeURIComponent(profile)}&session=${session}`);
      }
      transcript.message("user", text);
      input.value = "";
      await client.request("prompt.submit", { session_id: session, text, files: files.splice(0) });
      attached.textContent = "";
    } catch (err) {
      problem.replaceChildren(notice((err as Error).message, "error"));
    }
  });
  return page(
    "Chat",
    h("p", { class: "muted" }, `Profile ${profile}${session ? ` · session ${session}` : " · new session"}`),
    canDrive() ? null : notice("You are signed in as an observer: you can follow sessions but not send messages or answer approvals.", "warn"),
    transcript.el,
    problem,
    form,
  );
}
