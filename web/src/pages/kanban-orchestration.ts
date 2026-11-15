// The kanban page's orchestration controls (spec: web-dashboard, Kanban
// dashboard API and orchestration modes): the Auto/Manual switch bound to
// `kanban.auto_decompose` and the settings panel, both saved with
// `PUT /api/kanban/orchestration` into the dispatcher profile's
// config.toml. The panel shows what config.toml sets next to what
// applies; an unknown profile name is refused by the daemon (400) and
// nothing is saved.

import { api, ApiError } from "../lib/api";
import { h, notice, table } from "../lib/dom";
import { canDrive } from "../lib/state";

type Setting = "auto_decompose" | "auto_promote_children" | "orchestrator_profile" | "default_assignee";

export interface Orchestration {
  profile: string;
  configured: Record<Setting, boolean | string | null>;
  effective: Record<Setting, boolean | string | null> & { mode: "auto" | "manual" };
  problems: string[];
  orchestrators: string[];
  assignees: string[];
}

const ROWS: [Setting, string][] = [
  ["auto_decompose", "Automatic orchestration (kanban.auto_decompose)"],
  ["auto_promote_children", "Promote decomposed children (kanban.auto_promote_children)"],
  ["orchestrator_profile", "Orchestrator profile (kanban.orchestrator_profile)"],
  ["default_assignee", "Default assignee (kanban.default_assignee)"],
];

function shown(value: boolean | string | null | undefined): string {
  if (value === null || value === undefined) return "(default)";
  return String(value);
}

/** Saves `changes`; the daemon's answer, or `null` after showing why not. */
async function save(changes: Partial<Record<Setting, unknown>>, status: HTMLElement): Promise<Orchestration | null> {
  try {
    const saved = await api.put<Orchestration>("/api/kanban/orchestration", changes);
    status.replaceChildren(notice("Orchestration settings saved."));
    return saved;
  } catch (err) {
    status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    return null;
  }
}

/** The Auto/Manual switch. */
function modeControl(o: Orchestration, status: HTMLElement, redraw: (o: Orchestration) => void): HTMLElement {
  const auto = o.effective.mode === "auto";
  const button = (mode: "auto" | "manual", label: string): HTMLButtonElement => {
    const b = h("button", { type: "button", name: `mode-${mode}`, "aria-pressed": String(o.effective.mode === mode), disabled: !canDrive() }, label);
    b.addEventListener("click", async () => {
      const saved = await save({ auto_decompose: mode === "auto" }, status);
      if (saved) redraw(saved);
    });
    return b;
  };
  return h(
    "div",
    { class: "orchestration-mode", role: "group", "aria-label": "Orchestration", "data-mode": o.effective.mode },
    h("strong", {}, "Orchestration: "),
    button("auto", "Auto"),
    " ",
    button("manual", "Manual"),
    h("p", { class: "muted" }, auto ? "The dispatcher specifies triage tasks and decomposes orchestrator tasks before dispatching." : "Specify and decompose run only when asked (prometheus kanban specify / decompose)."),
  );
}

function profileField(name: Setting, value: string | null, choices: string[]): HTMLElement {
  const list = `kanban-${name}-choices`;
  return h(
    "span",
    {},
    h("input", { name, value: value ?? "", list, placeholder: "(unset)", autocomplete: "off" }),
    h("datalist", { id: list }, ...choices.map((c) => h("option", { value: c }))),
  );
}

/** The settings panel: configured versus effective, and a save form. */
function settingsPanel(o: Orchestration, status: HTMLElement, redraw: (o: Orchestration) => void): HTMLElement {
  const form = h(
    "form",
    { class: "orchestration-settings", "aria-label": "Orchestration settings" },
    h("label", {}, "Orchestrator profile ", profileField("orchestrator_profile", o.configured.orchestrator_profile as string | null, o.orchestrators.length ? o.orchestrators : o.assignees)),
    h("label", {}, "Default assignee ", profileField("default_assignee", o.configured.default_assignee as string | null, o.assignees)),
    h("label", {}, h("input", { type: "checkbox", name: "auto_promote_children", checked: o.effective.auto_promote_children === true }), " Promote decomposed children"),
    h("button", { type: "submit", name: "save", disabled: !canDrive() }, "Save"),
  );
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const data = new FormData(form);
    const saved = await save(
      {
        orchestrator_profile: String(data.get("orchestrator_profile") ?? "").trim(),
        default_assignee: String(data.get("default_assignee") ?? "").trim(),
        auto_promote_children: data.get("auto_promote_children") === "on",
      },
      status,
    );
    if (saved) redraw(saved);
  });
  const values = table(
    ["Setting", "Configured", "Effective"],
    ROWS.map(([key, label]) => [label, shown(o.configured[key]), shown(o.effective[key])]),
  );
  values.classList.add("orchestration-values");
  return h(
    "div",
    {},
    h("p", { class: "muted" }, `Settings of the dispatcher's profile ${o.profile}.`),
    values,
    ...o.problems.map((p) => notice(p, "warn")),
    form,
  );
}

/** The orchestration section; it redraws itself after each save. */
export function orchestrationSection(initial: Orchestration): HTMLElement {
  const status = h("div", { class: "status orchestration-status", role: "status" });
  const body = h("div", {});
  const section = h("section", { class: "orchestration", "aria-label": "Orchestration" }, h("h2", {}, "Orchestration"), status, body);
  const redraw = (o: Orchestration): void => {
    body.replaceChildren(modeControl(o, status, redraw), settingsPanel(o, status, redraw));
  };
  redraw(initial);
  return section;
}
