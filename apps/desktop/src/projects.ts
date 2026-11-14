// The Projects sidebar (spec: desktop-app, Repository discovery for the
// Projects sidebar; projects, Repository discovery): explicit projects,
// repositories of the profile's sessions and the repositories the
// daemon's background scan found, each with one-click project creation.
// Discovered rows go as soon as scanning is turned off. Each project
// lists its sessions; a session the `project` tool moved shows under its
// new project at once (`session.project`).

import type { ProjectsList, RepoRow, SessionList } from "../../../schema/rpc";
import { h, notice } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";

/** How often the sidebar looks again while a scan runs. */
const SCANNING_MS = 1000;

export class ProjectsSidebar {
  readonly el = h("section", { class: "projects", "aria-label": "Projects" });
  private timer = 0;

  constructor(
    private readonly client: DesktopClient,
    private readonly openFolder: (path: string) => void,
    private readonly openSession: (sessionId: string) => void = () => {},
  ) {}

  async load(): Promise<void> {
    window.clearTimeout(this.timer);
    delete this.el.dataset.ready;
    try {
      const listed = await this.client.call<ProjectsList>("projects.list", { profile: this.client.profile });
      const sessions = await this.client.call<SessionList>("session.list", { profile: this.client.profile }).catch((): SessionList => ({ sessions: [] }));
      this.render(listed, await this.discovery(), sessions);
      if (listed.scanning) this.timer = window.setTimeout(() => void this.load(), SCANNING_MS);
    } catch (err) {
      this.el.replaceChildren(h("h3", {}, "Projects"), notice((err as Error).message, "warn"));
    }
    this.el.dataset.ready = "true";
  }

  /** `projects.repo_scan_enabled` of the window's profile (unset: on). */
  private async discovery(): Promise<HTMLElement> {
    const got = await this.client.call<{ value: unknown }>("config.get", { profile: this.client.profile, key: "projects.repo_scan_enabled" }).catch(() => ({ value: null }));
    const toggle = h("input", { type: "checkbox", name: "repo-scan", checked: got.value !== false, "aria-label": "Discover repositories" });
    toggle.addEventListener("change", async () => {
      await this.client.call("config.patch", { profile: this.client.profile, changes: [{ key: "projects.repo_scan_enabled", value: toggle.checked }] });
      await this.load();
    });
    return h("label", { class: "repo-scan" }, toggle, " Discover repositories");
  }

  private render(listed: ProjectsList, discovery: HTMLElement, sessions: SessionList): void {
    const projects = listed.projects.map((p) => {
      const open = h("button", { type: "button", name: "open-project", "data-project": p.slug, disabled: !p.path }, p.name);
      open.addEventListener("click", () => p.path && this.openFolder(p.path));
      const own = sessions.sessions.filter((s) => s.project_id === p.id);
      const items = own.map((s) => {
        const chat = h("button", { type: "button", name: "project-session", "data-session": s.session_id }, s.title ?? s.session_id);
        chat.addEventListener("click", () => this.openSession(s.session_id));
        return h("li", { "data-session": s.session_id }, chat);
      });
      return h("li", { "data-project": p.slug }, open, items.length > 0 ? h("ul", { class: "project-sessions" }, ...items) : "");
    });
    this.el.replaceChildren(
      h("h3", {}, "Projects"),
      discovery,
      h("ul", { class: "project-list" }, ...projects),
      ...this.group("From sessions", "inferred", listed.inferred),
      ...this.group(listed.scanning ? "Discovered (scanning…)" : "Discovered", "discovered", listed.discovered),
    );
  }

  private group(title: string, kind: string, rows: RepoRow[]): HTMLElement[] {
    if (rows.length === 0) return [];
    const items = rows.map((r) => {
      const create = h("button", { type: "button", name: "create-project", "data-path": r.path }, "Create project");
      create.addEventListener("click", async () => {
        await this.client.call("projects.create", { profile: this.client.profile, path: r.path });
        await this.load();
      });
      return h("li", { "data-repo": r.path, "data-kind": kind }, h("span", { title: r.path }, r.name), " ", create);
    });
    return [h("h4", {}, title), h("ul", { class: `repos ${kind}` }, ...items)];
  }
}
