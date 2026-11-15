// `/kanban` (spec: web-dashboard, Kanban dashboard API and orchestration
// modes): the home's shared boards with their open tasks, the
// orchestration controls (./kanban-orchestration), the running workers
// with each run's details and a Terminate action that stops the worker
// and returns its task to `ready`, and the dispatcher's inspect view
// (backlog, in progress against its limit, recent events). Each open task
// takes an attachment (refused by the server above
// kanban.attachment_max_bytes). Observers see everything read-only.

import { api, ApiError } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { canDrive } from "../lib/state";
import { orchestrationSection, type Orchestration } from "./kanban-orchestration";

interface TaskRow {
  id: string;
  title: string;
  status: string;
  assignee: string | null;
  priority: number;
}

interface BoardRow {
  slug: string;
  name: string;
  tasks: TaskRow[];
}

interface Worker {
  worker_id: number | null;
  run_id: string;
  board: string;
  task_id: string;
  task_title: string;
  profile: string;
  role: string;
  started_at: string;
  last_heartbeat: string | null;
}

interface RunDetails {
  run_id: string;
  task_id: string;
  status: string;
  started_at: string;
  ended_at: string | null;
  exit_code: number | null;
  log_name: string;
  error: string | null;
}

interface Inspect {
  backlog: Record<string, number>;
  in_progress: number;
  in_progress_limit: number;
  paused: boolean;
  recent_events: { board: string; task_id: string | null; kind: string; actor: string; at: string }[];
}

function message(err: unknown): string {
  return err instanceof ApiError ? err.message : String(err);
}

/** The Attach control of one task: the chosen file is uploaded as is. */
function attachControl(board: string, task: string, status: HTMLElement): HTMLElement {
  const input = h("input", { type: "file", name: "attach", "aria-label": `Attach a file to ${task}`, disabled: !canDrive() });
  input.addEventListener("change", async () => {
    const file = input.files?.[0];
    if (!file) return;
    const path = `/api/kanban/boards/${encodeURIComponent(board)}/tasks/${encodeURIComponent(task)}/attachments?name=${encodeURIComponent(file.name)}`;
    try {
      const stored = await api.upload<{ filename: string; size: number }>(path, file);
      status.replaceChildren(notice(`Attached ${stored.filename} (${stored.size} bytes) to ${task}`));
    } catch (err) {
      status.replaceChildren(notice(message(err), "error"));
    }
    input.value = "";
  });
  return input;
}

function boardsSection(boards: BoardRow[], status: HTMLElement): HTMLElement {
  const items = boards.map((b) =>
    h(
      "div",
      { class: "kanban-board", "data-board": b.slug },
      h("h3", {}, b.name === b.slug ? b.slug : `${b.name} (${b.slug})`),
      b.tasks.length
        ? table(
            ["Task", "Title", "Status", "Assignee", "Attach"],
            b.tasks.map((t) => [t.id, t.title, h("span", { class: "task-status", "data-task": t.id }, t.status), t.assignee ?? "", attachControl(b.slug, t.id, status)]),
          )
        : notice("No open tasks."),
    ),
  );
  return h("section", { class: "kanban-boards", "aria-label": "Boards" }, h("h2", {}, "Boards"), ...items);
}

function runDetails(run: RunDetails): HTMLElement {
  const rows: [string, string][] = [
    ["Task", run.task_id],
    ["Status", run.status],
    ["Started", run.started_at],
    ["Ended", run.ended_at ?? ""],
    ["Exit code", run.exit_code === null ? "" : String(run.exit_code)],
    ["Log", run.log_name],
  ];
  if (run.error) rows.push(["Error", run.error]);
  return h("dl", { class: "run-details", "data-run": run.run_id }, ...rows.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));
}

/** The Details and Terminate buttons of one worker. */
function workerActions(w: Worker, status: HTMLElement, details: HTMLElement, reload: () => Promise<void>): HTMLElement {
  const show = h("button", { type: "button", name: "details" }, "Details");
  show.addEventListener("click", async () => {
    try {
      details.replaceChildren(runDetails(await api.get<RunDetails>(`/api/kanban/runs/${encodeURIComponent(w.run_id)}`)));
    } catch (err) {
      details.replaceChildren(notice(message(err), "error"));
    }
  });
  const stop = h("button", { type: "button", name: "terminate", disabled: !canDrive() }, "Terminate");
  stop.addEventListener("click", async () => {
    try {
      const done = await api.post<{ task_id: string; status: string }>(`/api/kanban/runs/${encodeURIComponent(w.run_id)}/terminate`, {});
      status.replaceChildren(notice(`Terminated ${w.run_id}; ${done.task_id} is ${done.status}`));
    } catch (err) {
      status.replaceChildren(notice(message(err), "error"));
    }
    await reload();
  });
  return h("span", { class: "worker-actions", "data-run": w.run_id }, show, " ", stop);
}

function workersSection(workers: Worker[], status: HTMLElement, reload: () => Promise<void>): HTMLElement {
  const details = h("div", { class: "run-panel" });
  const rows = workers.map((w) => [String(w.worker_id ?? ""), w.profile, `${w.task_id} ${w.task_title}`, w.started_at, w.last_heartbeat ?? "", workerActions(w, status, details, reload)]);
  return h(
    "section",
    { class: "kanban-workers", "aria-label": "Active workers" },
    h("h2", {}, "Active workers"),
    workers.length ? table(["Worker", "Profile", "Task", "Started", "Last heartbeat", ""], rows) : notice("No worker is running."),
    details,
  );
}

function inspectSection(i: Inspect): HTMLElement {
  const backlog = Object.entries(i.backlog).map(([s, n]) => [s, String(n)]);
  return h(
    "section",
    { class: "kanban-inspect", "aria-label": "Inspect" },
    h("h2", {}, "Inspect"),
    h("p", { class: "in-progress" }, `In progress: ${i.in_progress} of ${i.in_progress_limit}`, i.paused ? " (paused)" : ""),
    table(["Backlog", "Tasks"], backlog),
    h("h3", {}, "Recent events"),
    i.recent_events.length ? table(["When", "Board", "Task", "Event", "By"], i.recent_events.map((e) => [e.at, e.board, e.task_id ?? "", e.kind, e.actor])) : notice("No events yet."),
  );
}

export async function kanbanPage(): Promise<HTMLElement> {
  const status = h("div", { class: "status", role: "status" });
  const orchestration = h("div", {});
  const live = h("div", { class: "kanban-live" });
  const reload = async (): Promise<void> => {
    delete live.dataset.ready;
    try {
      const [boards, workers, inspect] = await Promise.all([
        api.get<{ boards: BoardRow[] }>("/api/kanban/boards"),
        api.get<{ workers: Worker[] }>("/api/kanban/workers/active"),
        api.get<Inspect>("/api/kanban/inspect"),
      ]);
      live.replaceChildren(workersSection(workers.workers, status, reload), boardsSection(boards.boards, status), inspectSection(inspect));
    } catch (err) {
      live.replaceChildren(notice(message(err), "error"));
    }
    live.dataset.ready = "true";
  };
  try {
    orchestration.replaceChildren(orchestrationSection(await api.get<Orchestration>("/api/kanban/orchestration")));
  } catch (err) {
    orchestration.replaceChildren(notice(message(err), "error"));
  }
  await reload();
  const refresh = h("button", { type: "button", name: "refresh" }, "Refresh");
  refresh.addEventListener("click", () => void reload());
  const hint = canDrive() ? null : notice("You are an observer: changes need a driver.", "info");
  return page("Kanban", hint, status, refresh, orchestration, live);
}
