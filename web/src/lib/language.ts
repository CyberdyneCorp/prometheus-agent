// The language switcher of the dashboard and the desktop (spec: i18n,
// Language switchers show endonyms): every language the profile can
// select (bundled, overlay and installed packs) listed by its own name
// with its id, the choice written to `display.language` with
// `config.patch`, and the page mirrored at once when the language is
// written right to left. Surfaces without a catalog for the language keep
// their English text.

import { h } from "./dom";

export interface LanguageEntry {
  id: string;
  endonym: string;
  rtl: boolean;
  origin: string;
}

export interface Languages {
  current: string;
  languages: LanguageEntry[];
}

/** A protocol call (the dashboard's or the desktop's client). */
export type Call = (method: string, params: unknown) => Promise<unknown>;

/** The option label: `العربية (ar)`. */
export function label(language: LanguageEntry): string {
  return `${language.endonym} (${language.id})`;
}

/** Mirrors the document for a right-to-left language, else restores
 *  left to right; also sets the document's language. */
export function applyDirection(languages: LanguageEntry[], id: string): void {
  const rtl = languages.find((language) => language.id === id)?.rtl ?? false;
  document.documentElement.dir = rtl ? "rtl" : "ltr";
  document.documentElement.lang = id;
}

export class LanguagePicker {
  readonly el: HTMLElement;
  readonly select: HTMLSelectElement;
  private listed: LanguageEntry[] = [];
  private readonly status: HTMLElement;

  constructor(
    private readonly call: Call,
    private readonly profile: () => string,
    readOnly = false,
  ) {
    this.select = h("select", { name: "language", "aria-label": "Language", disabled: readOnly });
    this.status = h("span", { class: "language-status", role: "status" });
    this.el = h("label", { class: "language-picker" }, "Language ", this.select, this.status);
    this.select.addEventListener("change", () => void this.choose(this.select.value));
  }

  /** Lists the profile's languages and applies its current direction. */
  async load(): Promise<void> {
    const got = (await this.call("i18n.languages", { profile: this.profile() })) as Languages;
    this.listed = got.languages;
    this.select.replaceChildren(...got.languages.map((language) => h("option", { value: language.id, selected: language.id === got.current }, label(language))));
    this.select.value = got.current;
    applyDirection(this.listed, got.current);
  }

  private async choose(id: string): Promise<void> {
    try {
      await this.call("config.patch", { profile: this.profile(), changes: [{ key: "display.language", value: id }] });
      applyDirection(this.listed, id);
      this.status.textContent = "";
    } catch (err) {
      this.status.textContent = `Not changed: ${(err as Error).message}`;
    }
  }
}
