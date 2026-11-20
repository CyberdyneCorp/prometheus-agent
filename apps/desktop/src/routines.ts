// A bot's routines (spec: bot-mode, Bot routines): ordinary scheduled
// jobs of the bot's own profile named `[bot:<name>] <routine>` and
// delivered into its Bot Chat. The pane lists them with
// `bot.routines.list` and adds, pauses, resumes and removes them through
// the same job changes the Scheduled jobs pane uses, so
// `prometheus -p <bot> cron list` shows exactly what the pane shows. The
// schedule picker can only produce strings of the `prometheus cron`
// grammar: five-field cron expressions and `every <n>m|h` intervals.

import type { BotRoutine } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import { type Cadence, DAYS, scheduleOf } from "./schedule";

/** What the pane asks of the daemon for one bot. */
export interface RoutineCalls {
  list: () => Promise<BotRoutine[]>;
  change: (change: "cron_create" | "cron_update" | "cron_delete", target: string | null, body: unknown) => Promise<void>;
}

/** The schedule picker: a cadence select plus the inputs it needs. */
class SchedulePicker {
  readonly el: HTMLElement;
  private readonly kind = h(
    "select",
    { name: "routine-cadence", "aria-label": "Repeats" },
    h("option", { value: "daily" }, "Every day"),
    h("option", { value: "weekdays" }, "Every weekday"),
    h("option", { value: "weekly" }, "Every week"),
    h("option", { value: "hours" }, "Every few hours"),
    h("option", { value: "minutes" }, "Every few minutes"),
  );
  private readonly time = h("input", { type: "time", name: "routine-time", value: "08:00", "aria-label": "At" });
  private readonly day = h("select", { name: "routine-day", "aria-label": "On" }, ...DAYS.map((d, i) => h("option", { value: String(i) }, d)));
  private readonly every = h("input", { type: "number", name: "routine-every", min: "1", value: "1", "aria-label": "Every" });
  private readonly preview = h("code", { class: "routine-schedule" });

  constructor() {
    for (const input of [this.kind, this.time, this.day, this.every]) input.addEventListener("input", () => this.sync());
    this.kind.addEventListener("change", () => this.sync());
    this.el = h("span", { class: "schedule-picker" }, this.kind, this.day, this.time, this.every, this.preview);
    this.sync();
  }

  cadence(): Cadence {
    const time = this.time.value;
    const every = Number(this.every.value);
    switch (this.kind.value) {
      case "weekdays":
        return { kind: "weekdays", time };
      case "weekly":
        return { kind: "weekly", day: Number(this.day.value), time };
      case "hours":
        return { kind: "hours", every };
      case "minutes":
        return { kind: "minutes", every };
      default:
        return { kind: "daily", time };
    }
  }

  private sync(): void {
    const kind = this.kind.value;
    this.time.hidden = kind === "hours" || kind === "minutes";
    this.every.hidden = !this.time.hidden;
    this.day.hidden = kind !== "weekly";
    this.preview.textContent = scheduleOf(this.cadence());
  }
}

/** The routines of one bot, with an add form. */
export class RoutinesPanel {
  readonly el = h("section", { class: "routines", "aria-label": "Routines" });

  constructor(
    private readonly bot: string,
    private readonly calls: RoutineCalls,
  ) {}

  async render(): Promise<void> {
    delete this.el.dataset.ready;
    let rows: HTMLElement[];
    try {
      rows = (await this.calls.list()).map((r) => this.row(r));
    } catch (err) {
      rows = [notice((err as Error).message, "error")];
    }
    this.el.replaceChildren(h("h4", {}, `Routines of ${this.bot}`), h("ul", { class: "routine-list" }, ...rows), this.form());
    this.el.dataset.ready = "true";
  }

  private row(routine: BotRoutine): HTMLElement {
    const paused = routine.state === "paused";
    const toggle = h("button", { type: "button", name: paused ? "resume-routine" : "pause-routine" }, paused ? "Resume" : "Pause");
    toggle.addEventListener("click", () => void this.act("cron_update", routine.job_id, { enabled: paused }, toggle));
    const remove = h("button", { type: "button", name: "remove-routine" }, "Remove");
    remove.addEventListener("click", () => void this.act("cron_delete", routine.job_id, {}, remove));
    const next = routine.next_run_at ? ` · next ${routine.next_run_at}` : "";
    return h("li", { "data-routine": routine.routine, "data-state": routine.state }, h("strong", {}, routine.routine), ` · ${routine.schedule} · ${routine.state}${next} `, toggle, remove);
  }

  private form(): HTMLElement {
    const name = h("input", { name: "routine-name", required: true, placeholder: "morning digest", "aria-label": "Routine name" });
    const prompt = h("textarea", { name: "routine-prompt", required: true, rows: 2, "aria-label": "What to do" });
    const picker = new SchedulePicker();
    const problem = h("p", { class: "routine-problem", role: "alert" });
    const form = h("form", { class: "routine-add" }, name, prompt, picker.el, h("button", { type: "submit", name: "add-routine" }, "Add routine"), problem);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const body = { name: `[bot:${this.bot}] ${name.value.trim()}`, prompt: prompt.value, schedule: scheduleOf(picker.cadence()), deliver: ["bot_chat"] };
      void this.act("cron_create", null, body, problem);
    });
    return form;
  }

  /** Makes the change and lists again; a refusal (a schedule the
   *  daemon rejects, a job already gone) shows next to `near`. */
  private async act(change: "cron_create" | "cron_update" | "cron_delete", target: string | null, body: unknown, near: HTMLElement): Promise<void> {
    try {
      await this.calls.change(change, target, body);
    } catch (err) {
      near.after(notice((err as Error).message, "error"));
      return;
    }
    await this.render();
  }
}
