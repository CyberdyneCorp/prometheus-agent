// `/logs`: the last records of one log of the daemon's registry, redacted
// by the daemon, at most 5000 lines (spec: web-dashboard, Logs page is
// redacted and bounded).

import { api } from "../lib/api";
import { clear, h, notice, page, table } from "../lib/dom";

interface LogRecord {
  ts: string;
  level: string;
  component: string;
  msg: string;
}

export async function logsPage(params: URLSearchParams): Promise<HTMLElement> {
  const files = (await api.get<{ files: string[] }>("/api/logs")).files;
  const chosen = params.get("file") ?? files[0] ?? "";
  const select = h("select", { name: "file", "aria-label": "Log" }, ...files.map((f) => h("option", { value: f, selected: f === chosen }, f)));
  const lines = h("input", { name: "lines", type: "number", min: 1, max: 5000, value: params.get("lines") ?? "200", "aria-label": "Lines" });
  const out = h("div");
  const load = async () => {
    clear(out);
    try {
      const data = await api.get<{ records: LogRecord[] }>(`/api/logs?file=${encodeURIComponent(select.value)}&lines=${lines.value}`);
      out.append(data.records.length ? table(["Time", "Level", "Message"], data.records.map((r) => [r.ts, r.level, r.msg])) : notice("No records."));
    } catch (err) {
      out.append(notice((err as Error).message, "error"));
    }
  };
  const form = h("form", { class: "toolbar" }, select, lines, h("button", { type: "submit" }, "Show"));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    void load();
  });
  if (chosen) await load();
  return page("Logs", form, out);
}
