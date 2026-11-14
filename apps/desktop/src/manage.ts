// The management panes (spec: desktop-app, Management panes): Skills,
// Scheduled jobs (with an `All profiles` view), Profiles, Messaging,
// Agents and the Command Center. Every request names the profile it
// targets: the one chosen in `Applies to` (hidden with fewer than two
// profiles, back to the window's profile when that changes), or, in the
// all-profiles jobs view, the job's owning profile.

import type { CenterList } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { MessagingPane } from "./messaging";
import type { ProfileClients } from "./profiles";
import { shell } from "./shell";

type Page = "skills" | "jobs" | "profiles" | "messaging" | "agents" | "center" | "shortcuts";

const PAGES: [Page, string][] = [
  ["skills", "Skills"],
  ["jobs", "Scheduled jobs"],
  ["profiles", "Profiles"],
  ["messaging", "Messaging"],
  ["agents", "Agents"],
  ["center", "Command Center"],
  ["shortcuts", "Shortcuts"],
];

interface Job {
  id: string;
  name?: string | null;
  schedule: string;
  state: string;
  last_status?: string | null;
  next_run_at?: string | null;
}

/** What the panes do in the window beyond their own requests. */
export interface ManageActions {
  openSession: (profile: string, sessionId: string) => Promise<void>;
  switchProfile: (profile: string) => Promise<void>;
  exportProfile: (profile: string) => Promise<void>;
  importProfile: () => Promise<void>;
}

export class ManagePane {
  readonly el = h("section", { class: "manage", "aria-label": "Manage" });
  private readonly body = h("div", { class: "manage-page" });
  private readonly applies: HTMLSelectElement;
  private readonly messaging: MessagingPane;
  private page: Page = "skills";
  private allProfiles = false;
  private shortcuts: HTMLElement = h("div");
  /** Each render's number: a slower, older render never replaces a newer one. */
  private renders = 0;

  constructor(
    private readonly client: DesktopClient,
    private readonly pool: ProfileClients,
    private readonly profiles: string[],
    dialogs: HTMLElement,
    private readonly actions: ManageActions,
  ) {
    this.messaging = new MessagingPane(pool, dialogs);
    this.applies = h("select", { name: "manage-applies-to", "aria-label": "Applies to" }, ...profiles.map((p) => h("option", { value: p }, p)));
    this.applies.addEventListener("change", () => void this.show(this.page));
    const nav = h(
      "nav",
      { class: "manage-nav" },
      ...PAGES.map(([page, label]) => {
        const b = h("button", { type: "button", name: "manage-page", "data-page": page }, label);
        b.addEventListener("click", () => void this.show(page));
        return b;
      }),
    );
    const applies = profiles.length >= 2 ? h("label", { class: "applies-to" }, "Applies to ", this.applies) : null;
    this.el.append(h("h2", {}, "Manage"), nav, applies ?? "", this.body);
  }

  /** The window's shortcuts panel (it lists every bindable action). */
  setShortcuts(panel: HTMLElement): void {
    this.shortcuts = panel;
  }

  /** The window's profile changed: `Applies to` follows it. */
  reset(): void {
    this.applies.value = this.client.profile;
    void this.show(this.page);
  }

  private get target(): string {
    return this.applies.value || this.client.profile;
  }

  async show(page: Page): Promise<void> {
    const render = ++this.renders;
    this.page = page;
    delete this.body.dataset.ready;
    let content: HTMLElement;
    try {
      content = await this.render(page);
    } catch (err) {
      content = notice((err as Error).message, "error");
    }
    if (render !== this.renders) return;
    this.body.dataset.page = page;
    this.body.replaceChildren(content);
    this.body.dataset.ready = "true";
  }

  private async render(page: Page): Promise<HTMLElement> {
    switch (page) {
      case "skills":
        return this.skills();
      case "jobs":
        return this.jobs();
      case "profiles":
        return this.profilesPage();
      case "messaging":
        await this.messaging.show(this.target);
        return this.messaging.el;
      case "agents":
        return this.agents();
      case "center":
        return this.center();
      default:
        return this.shortcuts;
    }
  }

  private async change(profile: string, change: string, target: string | null, body: unknown = {}): Promise<void> {
    await this.pool.call(profile, "manage.change", { profile, change, target, body });
    await this.show(this.page);
  }

  private async skills(): Promise<HTMLElement> {
    const profile = this.target;
    const installed = await this.pool.call<{ skills: { name: string; enabled: boolean }[] }>(profile, "manage.catalog", { profile, catalog: "skills" });
    const optional = await this.pool.call<{ skills: { name: string; id: string; installed: boolean }[] }>(profile, "manage.catalog", { profile, catalog: "optional_skills" });
    const rows = installed.skills.map((s) => {
      const toggle = h("input", { type: "checkbox", name: "skill-enabled", "data-skill": s.name, checked: s.enabled, "aria-label": `Enable ${s.name}` });
      toggle.addEventListener("change", () => void this.change(profile, "skill", s.name, { enabled: toggle.checked }));
      return h("li", {}, h("label", {}, toggle, ` ${s.name}`));
    });
    const catalog = optional.skills
      .filter((s) => !s.installed)
      .map((s) => {
        const install = h("button", { type: "button", name: "install-skill", "data-skill": s.id }, "Install");
        install.addEventListener("click", async () => {
          install.disabled = true;
          await shell("catalog_install", { kind: "skill", id: s.id, profile }).catch((err: unknown) => window.alert(String(err)));
          await this.show("skills");
        });
        return h("li", {}, `${s.name} `, install);
      });
    return h("div", {}, h("h3", {}, "Installed skills"), h("ul", { class: "skills-installed" }, ...rows), h("h3", {}, "Optional catalog"), h("ul", { class: "skills-optional" }, ...catalog));
  }

  private async jobs(): Promise<HTMLElement> {
    const all = h("input", { type: "checkbox", name: "all-profiles", checked: this.allProfiles, "aria-label": "All profiles" });
    all.addEventListener("change", () => {
      this.allProfiles = all.checked;
      void this.show("jobs");
    });
    const owners = this.allProfiles ? this.profiles : [this.target];
    const rows: HTMLElement[] = [];
    for (const owner of owners) {
      const listed = await this.pool.call<{ jobs: Job[] }>(owner, "manage.catalog", { profile: owner, catalog: "cron" });
      rows.push(...listed.jobs.map((job) => this.job(owner, job)));
    }
    return h("div", {}, h("label", {}, all, " All profiles"), h("ul", { class: "jobs" }, ...rows));
  }

  /** One job; every action targets its owning profile. */
  private job(owner: string, job: Job): HTMLElement {
    const paused = job.state === "paused";
    const pause = h("button", { type: "button", name: paused ? "resume-job" : "pause-job" }, paused ? "Resume" : "Pause");
    pause.addEventListener("click", () => void this.change(owner, "cron_update", job.id, { enabled: paused }));
    const run = h("button", { type: "button", name: "run-job" }, "Run now");
    run.addEventListener("click", () => void this.change(owner, "cron_run", job.id));
    const history = h("ol", { class: "job-runs" });
    const more = h("details", { class: "job-history" }, h("summary", {}, "Run history"), history);
    more.addEventListener("toggle", async () => {
      if (!more.open) return;
      const runs = await this.pool.call<{ runs: { status: string; scheduled_at: number; error?: string | null }[] }>(owner, "manage.catalog", { profile: owner, catalog: "cron_runs", target: job.id });
      history.replaceChildren(...runs.runs.map((r) => h("li", {}, `${new Date(r.scheduled_at).toISOString()} ${r.status}${r.error ? `: ${r.error}` : ""}`)));
    });
    return h("li", { "data-job": job.id, "data-owner": owner, "data-state": job.state }, h("strong", {}, job.name ?? job.id), ` ${owner} · ${job.schedule} · ${job.state} `, pause, run, more);
  }

  private profilesPage(): HTMLElement {
    const rows = this.profiles.map((profile) => {
      const use = h("button", { type: "button", name: "use-profile", "data-profile": profile, disabled: profile === this.client.profile }, "Switch");
      use.addEventListener("click", () => void this.actions.switchProfile(profile));
      const exported = h("button", { type: "button", name: "export-profile", "data-profile": profile }, "Export");
      exported.addEventListener("click", () => void this.actions.exportProfile(profile));
      return h("li", { "data-profile": profile }, `${profile}${profile === this.client.profile ? " (active)" : ""} `, use, exported);
    });
    const imported = h("button", { type: "button", name: "import-profile" }, "Import profile…");
    imported.addEventListener("click", () => void this.actions.importProfile());
    return h("div", {}, h("ul", { class: "profiles" }, ...rows), imported);
  }

  /** Each profile as an agent: opening it opens its bot chat. */
  private agents(): HTMLElement {
    const rows = this.profiles.map((profile) => {
      const open = h("button", { type: "button", name: "open-agent", "data-profile": profile }, `@${profile}`);
      open.addEventListener("click", async () => {
        await this.actions.switchProfile(profile);
        const chat = await this.client.callAcrossRestart<{ session_id: string }>("bot.chat.ensure", { profile });
        await this.actions.openSession(profile, chat.session_id);
      });
      return h("li", {}, open);
    });
    return h("ul", { class: "agents" }, ...rows);
  }

  /** Every profile's running sessions, approvals and subagents. */
  private async center(): Promise<HTMLElement> {
    const listed: CenterList = { sessions: [], approvals: [], subagents: [] };
    for (const profile of this.profiles) {
      const one = await this.pool.call<CenterList>(profile, "center.list", {}).catch((): CenterList => ({ sessions: [], approvals: [], subagents: [] }));
      listed.sessions.push(...one.sessions);
      listed.approvals.push(...one.approvals);
      listed.subagents.push(...one.subagents);
    }
    const open = (profile: string, sessionId: string, label = "Open") => {
      const b = h("button", { type: "button", name: "center-open" }, label);
      b.addEventListener("click", () => void this.actions.openSession(profile, sessionId));
      return b;
    };
    const stop = (profile: string, sessionId: string) => {
      const b = h("button", { type: "button", name: "center-stop" }, "Stop");
      b.addEventListener("click", async () => {
        // Stopping drives the session: attach as a driver for the call.
        const owner = await this.pool.of(profile);
        await owner.call("session.attach", { session_id: sessionId });
        await owner.call("session.interrupt", { session_id: sessionId });
        await owner.call("session.detach", { session_id: sessionId });
        await this.show("center");
      });
      return b;
    };
    return h(
      "div",
      {},
      h("h3", {}, "Running sessions"),
      h("ul", { class: "center-sessions" }, ...listed.sessions.map((s) => h("li", { "data-session": s.session_id }, `${s.profile} · ${s.title ?? s.session_id} `, open(s.profile, s.session_id), stop(s.profile, s.session_id)))),
      h("h3", {}, "Pending approvals"),
      h("ul", { class: "center-approvals" }, ...listed.approvals.map((a) => h("li", { "data-request": a.request_id }, `${a.profile} · ${a.summary} `, open(a.profile, a.session_id, "Answer")))),
      h("h3", {}, "Active subagents"),
      h("ul", { class: "center-subagents" }, ...listed.subagents.map((s) => h("li", { "data-subagent": s.subagent_id }, `${s.profile} · ${s.goal} `, open(s.profile, s.session_id)))),
    );
  }
}
