// The preview agent (spec: desktop-app, Preview pane tool; Driving the
// preview page; Annotating the preview page): the part of the frame agent
// that lets the desktop UI tools read, drive and mark the preview page.
// The app (the frame's parent) asks with `{prometheus: "preview", id, op,
// args}` and gets `{prometheus: "preview.result", id, result}`; each new
// document says `preview.ready` with its address, which is how the app
// learns about navigations. Elements are addressed by refs (`e1`, `e2`...)
// kept on the elements themselves, so they survive re-renders of the rest
// of the page; a navigation starts a new document with none.

type Args = Record<string, unknown>;
type Result = Record<string, unknown>;

const REF = "data-prometheus-ref";
const INTERACTIVE = "a[href], button, input, select, textarea, summary, [role=button], [role=link], [role=checkbox], [role=tab], [contenteditable=true], [onclick]";
const MARKS = "prometheus-marks";

let nextRef = 1;
/** What each ref looked like at the last answer (for deltas). */
const seen = new Map<string, string>();
/** A selector per ref, to re-find an element the page re-created. */
const selectors = new Map<string, string>();

function visible(el: Element): boolean {
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== "hidden";
}

function nameOf(el: Element): string {
  const input = el as HTMLInputElement;
  const label = el.getAttribute("aria-label") ?? (input.labels?.[0]?.textContent || null) ?? input.placeholder ?? "";
  return (label || (el.textContent ?? "") || input.value || "").replace(/\s+/g, " ").trim().slice(0, 80);
}

function summary(el: Element): Result {
  const input = el as HTMLInputElement;
  const row: Result = { ref: el.getAttribute(REF), tag: el.tagName.toLowerCase(), name: nameOf(el) };
  if (input.type && el.tagName === "INPUT") row.type = input.type;
  const href = el.getAttribute("href");
  if (href) row.href = href;
  if (input.type !== "password" && "value" in input && input.value) row.value = String(input.value).slice(0, 80);
  return row;
}

function cssPath(el: Element): string {
  const parts: string[] = [];
  let node: Element | null = el;
  while (node && node !== document.documentElement) {
    if (node.id) {
      parts.unshift(`#${CSS.escape(node.id)}`);
      break;
    }
    const parent: Element | null = node.parentElement;
    const tag = node.tagName.toLowerCase();
    const same = parent ? Array.from(parent.children).filter((c) => c.tagName === node?.tagName) : [];
    parts.unshift(same.length > 1 ? `${tag}:nth-of-type(${same.indexOf(node) + 1})` : tag);
    node = parent;
  }
  return parts.join(" > ");
}

/** Gives every interactive element a ref; the visible ones (all with
 *  `full`), in document order. */
function interactive(full: boolean): Element[] {
  const all = Array.from(document.querySelectorAll(INTERACTIVE));
  for (const el of all) {
    if (el.hasAttribute(REF)) continue;
    const ref = `e${nextRef++}`;
    el.setAttribute(REF, ref);
    selectors.set(ref, cssPath(el));
  }
  return full ? all : all.filter(visible);
}

function snapshot(): void {
  seen.clear();
  for (const el of interactive(true)) seen.set(el.getAttribute(REF) ?? "", JSON.stringify(summary(el)));
}

/** What changed since the last answer. */
function delta(): Result {
  const before = new Map(seen);
  const rebound: string[] = [];
  for (const ref of before.keys()) {
    if (document.querySelector(`[${REF}="${ref}"]`)) continue;
    const again = document.querySelector(selectors.get(ref) ?? ":not(*)");
    if (again && !again.hasAttribute(REF)) {
      again.setAttribute(REF, ref);
      rebound.push(ref);
    }
  }
  const now = new Map(interactive(true).map((el) => [el.getAttribute(REF) ?? "", JSON.stringify(summary(el))]));
  const added = [...now.keys()].filter((r) => !before.has(r)).map((r) => JSON.parse(now.get(r) ?? "{}") as Result);
  const removed = [...before.keys()].filter((r) => !now.has(r));
  const changed = [...now.keys()].filter((r) => before.has(r) && before.get(r) !== now.get(r) && !rebound.includes(r)).map((r) => JSON.parse(now.get(r) ?? "{}") as Result);
  snapshot();
  return { added, changed, removed, rebound };
}

function target(args: Args): Element | string {
  const ref = typeof args.ref === "string" ? args.ref : "";
  const selector = typeof args.selector === "string" ? args.selector : "";
  if (ref) {
    const el = document.querySelector(`[${REF}="${ref}"]`);
    if (el) return el;
    const again = document.querySelector(selectors.get(ref) ?? ":not(*)");
    if (again) {
      again.setAttribute(REF, ref);
      return again;
    }
    return `ref ${ref} is no longer on the page; call drive_preview elements again`;
  }
  try {
    return document.querySelector(selector) ?? `nothing matches the selector ${selector}`;
  } catch {
    return `${selector} is not a valid CSS selector`;
  }
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 250));

function typeInto(el: Element, args: Args): Result | null {
  const field = el as HTMLInputElement;
  if (field.type === "password" && args.approved !== true) return { needs_approval: true, field: cssPath(el) };
  field.focus();
  field.value = `${field.value ?? ""}${String(args.text ?? "")}`;
  field.dispatchEvent(new Event("input", { bubbles: true }));
  field.dispatchEvent(new Event("change", { bubbles: true }));
  if (args.submit === true) {
    if (field.form) field.form.requestSubmit();
    else field.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  }
  return null;
}

function press(args: Args): Result | null {
  const key = String(args.key ?? "");
  const focused = document.activeElement && document.activeElement !== document.body ? document.activeElement : null;
  const on = args.ref || args.selector ? target(args) : focused;
  if (typeof on === "string") return { success: false, error: on };
  if (!on && key.length === 1 && args.allow_shortcut !== true) {
    return { success: false, error: `pressing "${key}" on the page body may trigger a page shortcut; focus an element first or pass allow_shortcut: true` };
  }
  const at = on ?? document.body;
  for (const type of ["keydown", "keypress", "keyup"]) at.dispatchEvent(new KeyboardEvent(type, { key, bubbles: true }));
  return null;
}

function scroll(args: Args): void {
  if (args.to === "top") window.scrollTo(0, 0);
  else if (args.to === "bottom") window.scrollTo(0, document.documentElement.scrollHeight);
  else window.scrollBy(0, typeof args.amount === "number" ? args.amount : Math.round(innerHeight * 0.8));
}

function strobe(): number {
  const els = Array.from(document.querySelectorAll(`[${REF}]`)) as HTMLElement[];
  for (const el of els) {
    const before = el.style.outline;
    el.style.outline = "2px solid #f59f00";
    setTimeout(() => {
      el.style.outline = before;
    }, 1000);
  }
  return els.length;
}

const NAVIGATIONS: Record<string, () => void> = {
  back: () => history.back(),
  forward: () => history.forward(),
  reload: () => location.reload(),
};

async function act(args: Args): Promise<Result> {
  const action = String(args.action ?? "");
  if (seen.size === 0) snapshot();
  const navigate = NAVIGATIONS[action];
  if (navigate) {
    setTimeout(navigate, 0);
    return { success: true, action, navigating: true };
  }
  let refusal: Result | null = null;
  if (action === "press") refusal = press(args);
  else if (action === "scroll") scroll(args);
  else if (action === "strobe") return { success: true, action, strobed: strobe() };
  else {
    const el = target(args);
    if (typeof el === "string") return { success: false, error: el };
    if (action === "click") (el as HTMLElement).click();
    else if (action === "hover") for (const type of ["pointerover", "mouseover", "mouseenter"]) el.dispatchEvent(new MouseEvent(type, { bubbles: type !== "mouseenter" }));
    else if (action === "type") refusal = typeInto(el, args);
  }
  if (refusal) return refusal;
  await settle();
  return { success: true, action, url: location.href, ...delta() };
}

function elements(args: Args): Result {
  const max = typeof args.max === "number" ? args.max : 100;
  const list = interactive(args.full === true);
  snapshot();
  return { success: true, url: location.href, title: document.title, elements: list.slice(0, max).map(summary), total: list.length };
}

function read(args: Args): Result {
  const text = document.body?.innerText ?? "";
  const start = Math.max(0, Number(args.start ?? 0));
  const count = Math.min(20_000, Math.max(1, Number(args.count ?? 20_000)));
  const end = Math.min(text.length, start + count);
  return { kind: "browser", url: location.href, title: document.title, text: text.slice(start, end), start, end, total_chars: text.length };
}

// Marks: lasting outlines that follow their element.
const marks = new Map<Element, string>();

function layer(): HTMLElement {
  let host = document.getElementById(MARKS);
  if (!host) {
    host = document.createElement("div");
    host.id = MARKS;
    host.setAttribute("style", "position:fixed;inset:0;pointer-events:none;z-index:2147483647");
    document.documentElement.append(host);
  }
  return host;
}

function drawMarks(): void {
  const host = layer();
  host.replaceChildren(
    ...[...marks].map(([el, label]) => {
      const r = el.getBoundingClientRect();
      const box = document.createElement("div");
      box.className = "prometheus-mark";
      box.setAttribute("style", `position:fixed;left:${r.left}px;top:${r.top}px;width:${r.width}px;height:${r.height}px;outline:2px solid #e8590c;border-radius:3px`);
      if (label) {
        const tag = document.createElement("span");
        tag.textContent = label;
        tag.setAttribute("style", "position:absolute;top:-1.4em;left:0;background:#e8590c;color:#fff;font:12px sans-serif;padding:0 4px;border-radius:3px");
        box.append(tag);
      }
      return box;
    }),
  );
}

function mark(args: Args): Result {
  const action = String(args.action ?? "");
  if (action === "clear") marks.clear();
  else if (action === "hold") for (const el of interactive(false)) marks.set(el, el.getAttribute(REF) ?? "");
  else {
    const el = target(args);
    if (typeof el === "string") return { success: false, error: el };
    if (action === "add") marks.set(el, String(args.label ?? "").slice(0, 40));
    else marks.delete(el);
  }
  drawMarks();
  return { success: true, action, marks: marks.size };
}

const OPS: Record<string, (args: Args) => Result | Promise<Result>> = { read, elements, act, mark };

/** Starts the preview agent in a frame (never in the app's own page). */
export function installPreviewAgent(): void {
  if (window.top === window) return;
  window.addEventListener("message", (event) => {
    const data = event.data as { prometheus?: string; id?: number; op?: string; args?: Args } | null;
    if (event.source !== window.parent || data?.prometheus !== "preview") return;
    const run = OPS[data.op ?? ""];
    const reply = (result: Result) => window.parent.postMessage({ prometheus: "preview.result", id: data.id, result }, "*");
    if (!run) return reply({ success: false, error: `unknown preview operation ${data.op}` });
    void Promise.resolve(run(data.args ?? {})).then(reply, (err: unknown) => reply({ success: false, error: String(err) }));
  });
  for (const type of ["scroll", "resize"]) window.addEventListener(type, () => marks.size > 0 && drawMarks(), true);
  const ready = () => window.parent.postMessage({ prometheus: "preview.ready", url: location.href, title: document.title }, "*");
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", ready);
  else ready();
}
