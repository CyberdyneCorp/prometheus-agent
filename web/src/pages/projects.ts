// The Projects page (spec: projects, Repository discovery): the profile's
// explicit projects, the repositories of its sessions and the ones the
// daemon's background scan found, each offered as one-click project
// creation over the client protocol (`projects.list`, `projects.create`).
// The dashboard only scans when the profile turns scanning on; it always
// shows what the scan cache holds (a desktop scan included).

import type { ProjectsList, RepoRow } from "../../../schema/rpc";
import { h, notice, page } from "../lib/dom";
import { rpc } from "../lib/rpc";
import { canDrive, profileOf } from "../lib/state";

/** How often the page looks again while a scan runs. */
const SCANNING_MS = 1000;

export async function projectsPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const body = h("div", { class: "projects-page" });
  const view = page("Projects", body);
  const load = async (): Promise<void> => {
    delete body.dataset.ready;
    try {
      const client = await rpc();
      const listed = (await client.call("projects.list", { profile })) as ProjectsList;
      body.replaceChildren(...render(listed, async (path) => {
        await client.call("projects.create", { profile, path });
        await load();
      }));
      // Refreshes while the scan runs, as long as the page is shown.
      if (listed.scanning) window.setTimeout(() => view.isConnected && void load(), SCANNING_MS);
    } catch (err) {
      body.replaceChildren(notice((err as Error).message, "error"));
    }
    body.dataset.ready = "true";
  };
  await load();
  return view;
}

function render(listed: ProjectsList, create: (path: string) => Promise<void>): HTMLElement[] {
  const projects = listed.projects.map((p) => h("li", { "data-project": p.slug }, p.name, p.path ? h("span", { class: "path" }, ` ${p.path}`) : null));
  return [
    h("h2", {}, "Projects"),
    projects.length ? h("ul", { class: "project-list" }, ...projects) : notice("No projects yet."),
    ...group("From sessions", "inferred", listed.inferred, create),
    ...group(listed.scanning ? "Discovered (scanning…)" : "Discovered", "discovered", listed.discovered, create),
  ];
}

function group(title: string, kind: string, rows: RepoRow[], create: (path: string) => Promise<void>): HTMLElement[] {
  if (rows.length === 0) return [];
  const items = rows.map((r) => {
    const button = h("button", { type: "button", name: "create-project", "data-path": r.path, disabled: !canDrive() }, "Create project");
    button.addEventListener("click", () => void create(r.path));
    return h("li", { "data-repo": r.path, "data-kind": kind }, h("span", { title: r.path }, r.name), " ", button);
  });
  return [h("h2", {}, title), h("ul", { class: `repos ${kind}` }, ...items)];
}
