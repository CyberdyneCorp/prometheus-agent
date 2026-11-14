// `/files` (spec: web-dashboard, Files page): the profile's workspace roots
// with list, preview (text up to 2 MiB, images), download, upload, rename
// and delete after confirmation. The daemon resolves every path and
// refuses those that leave the chosen root (403), links included.

import { api, ApiError, base, profilePath } from "../lib/api";
import { h, notice, page, table } from "../lib/dom";
import { href } from "../lib/router";
import { canDrive, profileOf } from "../lib/state";

interface Root {
  id: string;
  path: string;
}

interface Entry {
  name: string;
  kind: "file" | "dir";
  size: number;
}

function link(profile: string, root: string, path: string, label: string): HTMLElement {
  const query = new URLSearchParams({ profile, root, path });
  return h("a", { href: href(`/files?${query.toString()}`) }, label);
}

function join(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

async function preview(profile: string, root: string, path: string): Promise<HTMLElement> {
  const query = new URLSearchParams({ root, path });
  const url = `${base}${profilePath(profile, `/files/read?${query.toString()}`)}`;
  const download = h("a", { href: `${url}&download=1`, download: path.split("/").pop() ?? "file" }, "Download");
  if (/\.(png|jpe?g|gif|webp)$/i.test(path)) return h("div", { class: "preview" }, download, h("img", { src: url, alt: path }));
  try {
    const response = await fetch(url, { credentials: "same-origin" });
    if (!response.ok) return h("div", {}, download, notice(((await response.json()) as { message?: string }).message ?? `HTTP ${response.status}`, "error"));
    return h("div", { class: "preview" }, download, h("pre", { "data-preview": path }, await response.text()));
  } catch (err) {
    return notice(String(err), "error");
  }
}

function controls(profile: string, root: string, dir: string, entry: Entry, reload: () => void, status: HTMLElement): HTMLElement {
  const path = join(dir, entry.name);
  const query = new URLSearchParams({ root });
  const rename = h("button", { type: "button", disabled: !canDrive() }, "Rename");
  rename.addEventListener("click", async () => {
    const to = window.prompt(`Rename ${entry.name} to`, entry.name);
    if (!to || to === entry.name) return;
    try {
      await api.post(profilePath(profile, `/files/rename?${query.toString()}`), { path, to });
      reload();
    } catch (err) {
      status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    }
  });
  const remove = h("button", { type: "button", disabled: !canDrive() }, "Delete");
  remove.addEventListener("click", async () => {
    if (!window.confirm(`Delete ${path}?`)) return;
    const target = new URLSearchParams({ root, path, confirm: "true" });
    try {
      await api.del(profilePath(profile, `/files?${target.toString()}`));
      reload();
    } catch (err) {
      status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    }
  });
  return h("span", { class: "actions" }, rename, remove);
}

export async function filesPage(params: URLSearchParams): Promise<HTMLElement> {
  const profile = profileOf(params);
  const roots = (await api.get<{ roots: Root[] }>(profilePath(profile, "/files"))).roots;
  const root = params.get("root") ?? roots[0]?.id ?? "workspace";
  const path = params.get("path") ?? "";
  const status = h("div", {});
  const body = h("div", {});
  const reload = () => void filesPage(params).then((fresh) => body.closest("section")?.replaceWith(fresh));
  const rootLinks = h("p", {}, ...roots.flatMap((r) => [link(profile, r.id, "", r.id), " "]));
  const query = new URLSearchParams({ root, path });
  try {
    const listing = await api.get<{ entries: Entry[] }>(profilePath(profile, `/files/list?${query.toString()}`));
    const rows = listing.entries.map((e) => [
      e.kind === "dir" ? link(profile, root, join(path, e.name), `${e.name}/`) : link(profile, root, join(path, e.name), e.name),
      e.kind,
      String(e.size),
      controls(profile, root, path, e, reload, status),
    ]);
    const upload = h("input", { type: "file", "aria-label": "Upload a file", disabled: !canDrive() });
    upload.addEventListener("change", async () => {
      const file = upload.files?.[0];
      if (!file) return;
      const target = new URLSearchParams({ root, path, name: file.name });
      try {
        await api.upload(profilePath(profile, `/files/upload?${target.toString()}`), file);
        reload();
      } catch (err) {
        status.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
      }
    });
    body.replaceChildren(table(["Name", "Kind", "Bytes", ""], rows), h("p", {}, "Upload: ", upload));
  } catch (err) {
    if (err instanceof ApiError && err.status === 400) {
      body.replaceChildren(await preview(profile, root, path));
    } else {
      body.replaceChildren(notice(err instanceof ApiError ? err.message : String(err), "error"));
    }
  }
  return page("Files", h("p", { class: "muted" }, `Profile ${profile}, root ${root}: /${path}`), rootLinks, status, body);
}
