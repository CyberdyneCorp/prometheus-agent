// The palette's pet list (spec: pets, Pet slash command; Pet gallery
// commands): the installed pets to adopt and the gallery's to install,
// the pet hidden, or one generated (`/hatch`).

import { h } from "../../../../web/src/lib/dom";
import type { DesktopClient } from "../client";

interface PetInfo {
  slug: string;
  name: string;
  author?: string;
  installed: boolean;
}

/** Opens the list; `changed` runs after the pet changed, `command` runs
 *  `/hatch` from its button. */
export async function petsDialog(client: DesktopClient, host: HTMLElement, changed: () => void, command: (name: string, args: string) => void): Promise<void> {
  const profile = client.profile;
  const status = h("p", { class: "pets-status", role: "status" }, "Reading the gallery…");
  const list = h("ul", { class: "pets-list" });
  const close = h("button", { type: "button", name: "close" }, "Close");
  const off = h("button", { type: "button", name: "pet-off" }, "Hide the pet");
  const describe = h("input", { type: "text", name: "hatch-description", placeholder: "Describe a new pet…", "aria-label": "New pet" });
  const hatch = h("button", { type: "button", name: "hatch" }, "Generate…");
  const box = h("div", { class: "pets-dialog", role: "dialog", "aria-label": "Pets" }, h("h3", {}, "Pets"), status, list, h("div", { class: "row" }, describe, hatch), off, close);
  const done = () => box.remove();
  close.addEventListener("click", done);
  off.addEventListener("click", () => {
    void client.call("pets.set", { profile, enabled: false }).then(changed);
    done();
  });
  hatch.addEventListener("click", () => {
    done();
    command("hatch", describe.value);
  });
  const row = (pet: PetInfo) => {
    const label = pet.installed ? "Adopt" : "Install";
    const button = h("button", { type: "button", name: `pet-${pet.slug}` }, label);
    button.addEventListener("click", async () => {
      button.disabled = true;
      try {
        if (!pet.installed) await client.call("pets.install", { profile, slug: pet.slug, select: true });
        else await client.call("pets.set", { profile, slug: pet.slug });
        changed();
        done();
      } catch (err) {
        status.textContent = (err as Error).message;
        button.disabled = false;
      }
    });
    return h("li", { "data-slug": pet.slug }, h("span", {}, `${pet.name} (${pet.slug})`), button);
  };
  host.append(box);
  const state = await client.call<{ installed: PetInfo[] }>("pets.state", { profile }).catch(() => ({ installed: [] as PetInfo[] }));
  list.replaceChildren(...state.installed.map(row));
  try {
    const gallery = await client.call<{ pets: PetInfo[] }>("pets.gallery", { profile, query: "" });
    list.append(...gallery.pets.filter((p) => !p.installed).map(row));
    status.textContent = "";
  } catch (err) {
    status.textContent = `The gallery is not reachable: ${(err as Error).message}`;
  }
}
