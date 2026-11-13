// The annotate agent (spec: desktop-app, Annotate mode in the in-app
// browser): installed in every frame of the app's webview (Tauri's
// initialization script for all frames), inert until the app page, its
// parent, starts it in the preview frame. In annotate mode a click picks
// an element and a drag picks a box; the agent describes the pick to the
// app: CSS selector, markup, layout-relevant computed styles and a
// snapshot, with password and hidden field values and attributes that
// look like keys or tokens redacted here, before anything leaves the page.

const REDACTED = "[redacted]";
const SECRET_NAME = /token|secret|key|auth|passw|session|csrf|bearer|credential/i;
const SECRET_VALUE = /^(sk-|ghp_|gho_|xox[abp]-|eyJ)|^[A-Za-z0-9_+/=-]{32,}$/;
const STYLES = ["display", "position", "width", "height", "margin", "padding", "font-size", "font-weight", "color", "background-color", "border", "flex-direction", "justify-content", "align-items", "grid-template-columns", "gap", "z-index", "overflow"];
const MAX_MARKUP = 6000;

interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** A unique-enough CSS selector of `el`. */
function selectorOf(el: Element): string {
  const parts: string[] = [];
  let node: Element | null = el;
  while (node && node !== document.documentElement) {
    if (node.id) {
      parts.unshift(`#${CSS.escape(node.id)}`);
      break;
    }
    const tag = node.tagName.toLowerCase();
    const parent: Element | null = node.parentElement;
    const same = parent ? Array.from(parent.children).filter((c) => c.tagName === node?.tagName) : [];
    parts.unshift(same.length > 1 ? `${tag}:nth-of-type(${same.indexOf(node) + 1})` : tag);
    node = parent;
  }
  return parts.join(" > ");
}

/** A copy of `el` with secrets removed: password and hidden values and
 *  attributes that look like keys or tokens. */
function redacted(el: Element): Element {
  const copy = el.cloneNode(true) as Element;
  const all = [copy, ...Array.from(copy.querySelectorAll("*"))];
  for (const node of all) {
    for (const attr of Array.from(node.attributes)) {
      if (SECRET_NAME.test(attr.name) || SECRET_VALUE.test(attr.value)) node.setAttribute(attr.name, REDACTED);
    }
    if (node instanceof HTMLInputElement && (node.type === "password" || node.type === "hidden")) node.setAttribute("value", REDACTED);
  }
  return copy;
}

/** The top-level subtree `el` is in: its ancestor under `body`. */
function topOf(el: Element): string {
  let node: Element = el;
  while (node.parentElement && node.parentElement !== document.body && node.parentElement !== document.documentElement) node = node.parentElement;
  return selectorOf(node);
}

function styles(el: Element): Record<string, string> {
  const computed = getComputedStyle(el);
  return Object.fromEntries(STYLES.map((name) => [name, computed.getPropertyValue(name)]));
}

/** A snapshot of the redacted element as a PNG data URL (`null` where the
 *  webview refuses to rasterize it). */
async function snapshot(copy: Element, rect: Rect): Promise<string | null> {
  const width = Math.max(1, Math.round(rect.width));
  const height = Math.max(1, Math.round(rect.height));
  const xhtml = new XMLSerializer().serializeToString(copy);
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}"><foreignObject width="100%" height="100%"><div xmlns="http://www.w3.org/1999/xhtml">${xhtml}</div></foreignObject></svg>`;
  try {
    const image = new Image();
    image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    canvas.getContext("2d")?.drawImage(image, 0, 0);
    return canvas.toDataURL("image/png");
  } catch {
    return null;
  }
}

async function describe(el: Element, box: Rect | null): Promise<Record<string, unknown>> {
  const bounds = el.getBoundingClientRect();
  const rect = box ?? { x: bounds.x, y: bounds.y, width: bounds.width, height: bounds.height };
  const copy = redacted(el);
  return {
    prometheus: "annotate.pick",
    rect,
    selector: selectorOf(el),
    top: topOf(el),
    markup: copy.outerHTML.slice(0, MAX_MARKUP),
    styles: styles(el),
    snapshot: await snapshot(copy, rect),
  };
}

function install(): void {
  if (window.top === window) return;
  let active = false;
  let start: { x: number; y: number } | null = null;
  const send = (message: Record<string, unknown>) => window.parent.postMessage(message, "*");
  window.addEventListener("message", (event) => {
    const data = event.data as { prometheus?: string; action?: string } | null;
    if (event.source !== window.parent || data?.prometheus !== "annotate") return;
    active = data.action === "start";
    document.documentElement.style.cursor = active ? "crosshair" : "";
  });
  window.addEventListener(
    "mousedown",
    (event) => {
      if (!active) return;
      event.preventDefault();
      event.stopPropagation();
      start = { x: event.clientX, y: event.clientY };
    },
    true,
  );
  window.addEventListener(
    "mouseup",
    (event) => {
      if (!active || !start) return;
      event.preventDefault();
      event.stopPropagation();
      const dragged = Math.abs(event.clientX - start.x) > 4 || Math.abs(event.clientY - start.y) > 4;
      const box = dragged ? { x: Math.min(start.x, event.clientX), y: Math.min(start.y, event.clientY), width: Math.abs(event.clientX - start.x), height: Math.abs(event.clientY - start.y) } : null;
      const at = box ? { x: box.x + box.width / 2, y: box.y + box.height / 2 } : start;
      start = null;
      const el = document.elementFromPoint(at.x, at.y);
      if (el) void describe(el, box).then(send);
    },
    true,
  );
  // A click in annotate mode never activates the page.
  window.addEventListener(
    "click",
    (event) => {
      if (!active) return;
      event.preventDefault();
      event.stopPropagation();
    },
    true,
  );
}

install();
