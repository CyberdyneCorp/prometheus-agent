// Bot avatars (spec: bot-mode, Avatars): the daemon resolves a blob face
// to its seed and silhouette, and every client draws it from those two
// values alone, so two desktops showing the same bot draw the identical
// face. Drawing is a pure function of the avatar: no randomness, clock or
// window state.

import type { BotAvatar } from "../../../schema/rpc";

const SVG = "http://www.w3.org/2000/svg";
const SIZE = 48;

/** The geometric avatar's colours, by the names the daemon accepts. */
const COLOURS: Record<string, string> = {
  red: "#e03131",
  orange: "#f76707",
  amber: "#f59f00",
  green: "#2f9e44",
  teal: "#0ca678",
  cyan: "#1098ad",
  blue: "#1971c2",
  indigo: "#4263eb",
  purple: "#7048e8",
  pink: "#d6336c",
};

/** A small deterministic generator (mulberry32) seeded by the face seed. */
function generator(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function node(tag: string, attrs: Record<string, string | number>): SVGElement {
  const el = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attrs)) el.setAttribute(name, String(value));
  return el;
}

const round = (n: number) => Math.round(n * 10) / 10;

/** A closed path through `points` around the centre. */
function polygon(points: [number, number][]): string {
  return `${points.map(([x, y], i) => `${i === 0 ? "M" : "L"}${round(x)} ${round(y)}`).join(" ")} Z`;
}

/** Points on a circle of `radius(i)` around the centre. */
function ring(count: number, radius: (i: number) => number, turn = 0): [number, number][] {
  const c = SIZE / 2;
  return Array.from({ length: count }, (_, i) => {
    const a = turn + (i / count) * Math.PI * 2;
    return [c + Math.cos(a) * radius(i), c + Math.sin(a) * radius(i)];
  });
}

/** The blob's body outline, one per silhouette. */
const SILHOUETTES: Record<string, (next: () => number, fill: string) => SVGElement[]> = {
  round: (_next, fill) => [node("circle", { cx: 24, cy: 25, r: 19, fill })],
  organic: (next, fill) => [node("path", { d: polygon(ring(10, () => 16 + next() * 5)), fill })],
  boxy: (_next, fill) => [node("rect", { x: 6, y: 7, width: 36, height: 36, rx: 9, fill })],
  nub: (_next, fill) => [node("circle", { cx: 24, cy: 27, r: 17, fill }), node("circle", { cx: 24, cy: 9, r: 4, fill })],
  cloud: (_next, fill) => [16, 24, 32].map((cx, i) => node("circle", { cx, cy: i === 1 ? 20 : 27, r: i === 1 ? 14 : 12, fill })),
  sun: (_next, fill) => [node("path", { d: polygon(ring(16, (i) => (i % 2 === 0 ? 22 : 17))), fill })],
};

/** The default face: body colour, silhouette, eyes and mouth from the seed. */
function blob(seed: number, silhouette: string): SVGElement[] {
  const next = generator(seed);
  const fill = `hsl(${seed % 360} 62% 62%)`;
  const body = (SILHOUETTES[silhouette] ?? SILHOUETTES.round)(next, fill);
  const spread = 5 + next() * 3;
  const eyeY = 22 + next() * 3;
  const eyes = [-1, 1].map((side) => node("circle", { cx: round(24 + side * spread), cy: round(eyeY), r: 2.4, fill: "#1b1b1f" }));
  const smile = round(2 + next() * 4);
  const mouth = node("path", { d: `M19 ${round(eyeY + 7)} Q24 ${round(eyeY + 7 + smile)} 29 ${round(eyeY + 7)}`, stroke: "#1b1b1f", "stroke-width": 1.8, fill: "none", "stroke-linecap": "round" });
  return [...body, ...eyes, mouth];
}

/** The geometric avatar's shapes. */
const SHAPES: Record<string, (fill: string) => SVGElement> = {
  circle: (fill) => node("circle", { cx: 24, cy: 24, r: 18, fill }),
  square: (fill) => node("rect", { x: 7, y: 7, width: 34, height: 34, rx: 3, fill }),
  triangle: (fill) => node("path", { d: "M24 6 L42 40 L6 40 Z", fill }),
  diamond: (fill) => node("path", { d: "M24 4 L44 24 L24 44 L4 24 Z", fill }),
  hexagon: (fill) => node("path", { d: polygon(ring(6, () => 19, Math.PI / 6)), fill }),
  star: (fill) => node("path", { d: polygon(ring(10, (i) => (i % 2 === 0 ? 20 : 8), -Math.PI / 2)), fill }),
  heart: (fill) => node("path", { d: "M24 41 C10 31 5 23 8 15 C11 8 20 8 24 15 C28 8 37 8 40 15 C43 23 38 31 24 41 Z", fill }),
};

/** A monogram while an image or pet picture loads (or when it cannot). */
function monogram(name: string): SVGElement[] {
  const letter = node("text", { x: 24, y: 30, "text-anchor": "middle", "font-size": 18, fill: "#fff" });
  letter.textContent = (name[0] ?? "?").toUpperCase();
  return [node("circle", { cx: 24, cy: 24, r: 20, fill: "#495057" }), letter];
}

/** A pet's first idle frame: the atlas's top-left 192x208 cell. */
function petFrame(atlas: string): SVGElement {
  const frame = node("svg", { x: 0, y: 0, width: SIZE, height: SIZE, viewBox: "0 0 192 208" });
  frame.append(node("image", { href: atlas, width: 1536, height: 1872 }));
  return frame;
}

function parts(avatar: BotAvatar, name: string, picture: string | null): SVGElement[] {
  switch (avatar.kind) {
    case "blob":
      return blob(avatar.seed ?? 0, avatar.silhouette ?? "round");
    case "geometric":
      return [(SHAPES[avatar.shape] ?? SHAPES.circle)(COLOURS[avatar.color] ?? COLOURS.blue)];
    case "image":
      return picture ? [node("image", { href: picture, width: SIZE, height: SIZE, preserveAspectRatio: "xMidYMid slice" })] : monogram(name);
    case "pet":
      return picture ? [petFrame(picture)] : monogram(name);
    default:
      return monogram(name);
  }
}

/**
 * The bot's avatar as an inline SVG; a missing avatar draws a monogram.
 * `picture` is what an `image` or `pet` avatar shows, which the daemon
 * serves apart from the roster ([`avatarPicture`]): the image's data URL,
 * or the pet's atlas.
 */
export function drawAvatar(avatar: BotAvatar | null | undefined, name: string, picture: string | null = null): SVGSVGElement {
  const svg = node("svg", { viewBox: `0 0 ${SIZE} ${SIZE}`, width: 32, height: 32, class: "avatar", role: "img", "aria-label": `${name}'s avatar` }) as SVGSVGElement;
  svg.dataset.kind = avatar?.kind ?? "none";
  if (picture) svg.dataset.picture = "true";
  svg.append(...parts(avatar ?? { kind: "pet", id: "" }, name, picture));
  return svg;
}

/** Calls the daemon for a bot's profile. */
type BotCall = <T>(method: string, params: Record<string, unknown>) => Promise<T>;

/** The picture an `image` (`bot.avatar.get`) or `pet` (`pets.sprite`) avatar shows; null for the drawn kinds or when it cannot be read. */
export async function avatarPicture(avatar: BotAvatar | null | undefined, call: BotCall): Promise<string | null> {
  try {
    if (avatar?.kind === "image") {
      const picture = await call<{ mime: string; data: string }>("bot.avatar.get", {});
      return `data:${picture.mime};base64,${picture.data}`;
    }
    if (avatar?.kind === "pet") return (await call<{ atlas: string }>("pets.sprite", { slug: avatar.id })).atlas;
  } catch {
    // The monogram stays.
  }
  return null;
}
