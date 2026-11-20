// A pet drawn from its atlas (spec: pets, Pet storage and sprite format):
// one canvas showing the frames of the current state's row, at
// `pets.scale` of the native 192x208 cell. The atlas comes from the
// daemon (`pets.sprite`), validated there.

import { h } from "../../../../web/src/lib/dom";
import type { PetState } from "./state";

export interface SpriteData {
  slug: string;
  name: string;
  atlas: string;
  frame_width: number;
  frame_height: number;
  states: Record<string, { row: number; frames: number }>;
}

/** Frames per second. */
const FPS = 8;

export class Sprite {
  readonly el: HTMLCanvasElement;
  private image: HTMLImageElement | null = null;
  private data: SpriteData | null = null;
  private stateName: PetState = "idle";
  private frame = 0;
  private timer = 0;
  private scaleFactor = 0.33;

  constructor(label: string) {
    this.el = h("canvas", { class: "pet-sprite", role: "img", "aria-label": label });
  }

  /** Shows `data` (a new pet, or `null` for none). */
  async load(data: SpriteData | null): Promise<void> {
    this.data = data;
    if (!data) {
      this.image = null;
      return this.stop();
    }
    const image = new Image();
    image.src = data.atlas;
    await image.decode().catch(() => undefined);
    this.image = image;
    this.resize();
    this.start();
  }

  get slug(): string | null {
    return this.data?.slug ?? null;
  }

  set state(state: PetState) {
    if (state === this.stateName) return;
    this.stateName = state;
    this.frame = 0;
    this.el.dataset.state = state;
    this.draw();
  }

  get state(): PetState {
    return this.stateName;
  }

  set scale(scale: number) {
    this.scaleFactor = scale;
    this.resize();
  }

  get scale(): number {
    return this.scaleFactor;
  }

  private resize(): void {
    const data = this.data;
    if (!data) return;
    this.el.width = Math.max(1, Math.round(data.frame_width * this.scaleFactor));
    this.el.height = Math.max(1, Math.round(data.frame_height * this.scaleFactor));
    this.el.dataset.scale = this.scaleFactor.toFixed(2);
    this.draw();
  }

  private start(): void {
    this.stop();
    this.el.dataset.state = this.stateName;
    this.timer = window.setInterval(() => {
      this.frame += 1;
      this.draw();
    }, 1000 / FPS);
  }

  private stop(): void {
    window.clearInterval(this.timer);
  }

  /** The row and frame count shown for the state (an empty row: idle). */
  private row(): { row: number; frames: number } {
    const states = this.data?.states ?? {};
    const own = states[this.stateName];
    if (own && own.frames > 0) return own;
    return states.idle ?? { row: 0, frames: 1 };
  }

  private draw(): void {
    const ctx = this.el.getContext("2d");
    const data = this.data;
    if (!ctx || !data || !this.image) return;
    const { row, frames } = this.row();
    const column = frames > 0 ? this.frame % frames : 0;
    ctx.clearRect(0, 0, this.el.width, this.el.height);
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(this.image, column * data.frame_width, row * data.frame_height, data.frame_width, data.frame_height, 0, 0, this.el.width, this.el.height);
  }
}
