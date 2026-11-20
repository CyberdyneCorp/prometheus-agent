// The bot screen pane (spec: execution-environments, Bot screen; Bot
// screen takeover lease): the profile's virtual desktop streamed over RFB
// from the daemon's `/screen` endpoint, admitted with a single-use display
// ticket (`screen.ticket`) per connection. The view is read-only while the
// agent has control; Take over (`screen.takeover`) makes the agent wait
// and sends this window's pointer and keys to the screen until Hand back
// (`screen.handback`); while it has control, Open browser starts a
// browser on the screen with the agent's screen browser profile
// (`screen.browser`). Leaving the pane or losing the stream keeps the
// lease: a person hands the screen back explicitly (or it expires).

import type { ScreenLeaseState, ScreenTicket } from "../../../schema/rpc";
import { h } from "../../../web/src/lib/dom";
import type { DesktopClient } from "./client";
import { keysym, RfbClient, RfbError, type Surface } from "./rfb";

/** The name this window takes the screen over as (its client kind). */
const VIEWER = "desktop";
/** Close codes of `/screen` that need no Reconnect button. */
const CLOSED: Record<number, string> = {
  4404: "The bot screen is not running. Start it with `prometheus computer-use screen start`.",
  4401: "The display ticket was refused.",
  4403: "The display ticket was refused.",
};

type Control = "agent" | "you" | "other";

export class ScreenPane {
  readonly el = h("section", { class: "screen-pane", "aria-label": "Bot screen" });
  private readonly status = h("p", { class: "screen-status", role: "status" });
  private readonly takeOver = h("button", { type: "button", name: "screen-take-over" }, "Take over");
  private readonly handBack = h("button", { type: "button", name: "screen-hand-back", hidden: true }, "Hand back");
  private readonly reconnect = h("button", { type: "button", name: "screen-reconnect", hidden: true }, "Reconnect");
  private readonly browser = h("button", { type: "button", name: "screen-browser", hidden: true }, "Open browser");
  private readonly canvas = h("canvas", { class: "screen-canvas", tabindex: "0", "aria-label": "Bot screen display" });
  private socket: WebSocket | null = null;
  private rfb: RfbClient | null = null;
  private control: Control = "agent";
  private holder = "";
  private buttons = 0;

  constructor(private readonly client: DesktopClient) {
    this.takeOver.addEventListener("click", () => void this.lease("screen.takeover"));
    this.handBack.addEventListener("click", () => void this.lease("screen.handback"));
    this.reconnect.addEventListener("click", () => void this.connect());
    this.browser.addEventListener("click", () => void this.openBrowser());
    this.input();
    this.el.append(h("div", { class: "screen-bar" }, this.status, this.takeOver, this.handBack, this.browser, this.reconnect), this.canvas);
  }

  /** The pane is shown: connect unless already streaming. */
  show(): void {
    if (!this.socket) void this.connect();
  }

  private async connect(): Promise<void> {
    this.reconnect.hidden = true;
    this.say("Connecting to the bot screen…");
    let ticket: ScreenTicket;
    try {
      await this.client.ready();
      ticket = await this.client.call<ScreenTicket>("screen.ticket", {});
    } catch (err) {
      return this.lost(`Cannot reach the bot screen: ${(err as Error).message}`);
    }
    this.apply(ticket.lease);
    const url = new URL(this.client.url);
    url.pathname = ticket.path;
    url.search = "";
    const socket = new WebSocket(url.toString(), ["prometheus-display.v1", `prometheus-ticket.${ticket.ticket}`]);
    socket.binaryType = "arraybuffer";
    const rfb = new RfbClient(this.surface());
    rfb.onSend = (bytes) => socket.readyState === WebSocket.OPEN && socket.send(bytes);
    rfb.onReady = () => this.apply();
    socket.onmessage = (event) => {
      try {
        rfb.receive(new Uint8Array(event.data as ArrayBuffer));
      } catch (err) {
        if (!(err instanceof RfbError)) throw err;
        socket.close();
        this.lost(err.message);
      }
    };
    socket.onclose = (event) => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.rfb = null;
      const known = CLOSED[event.code];
      this.lost(known ?? "The bot screen stream ended.", !known);
    };
    this.socket = socket;
    this.rfb = rfb;
  }

  private lost(text: string, retry = true): void {
    this.el.dataset.stream = "closed";
    this.say(text);
    this.reconnect.hidden = !retry;
    this.takeOver.hidden = true;
    this.handBack.hidden = this.control !== "you";
  }

  /** Takes the screen over or hands it back. */
  private async lease(method: "screen.takeover" | "screen.handback"): Promise<void> {
    try {
      this.apply(await this.client.call<ScreenLeaseState>(method, { viewer: VIEWER }));
    } catch (err) {
      this.say((err as Error).message);
    }
  }

  /** Shows who controls the screen (`state`: as the daemon reported). */
  private apply(state?: ScreenLeaseState): void {
    if (state) {
      this.holder = state.holder ?? "";
      this.control = !state.holder ? "agent" : state.holder === VIEWER ? "you" : "other";
    }
    this.el.dataset.control = this.control;
    if (!this.rfb?.width) return;
    this.el.dataset.stream = "open";
    const texts: Record<Control, string> = {
      agent: "The agent has control. Take over to use the screen yourself; the agent waits.",
      you: "You have control. The agent waits until you hand the screen back.",
      other: `${this.holder} has control; taking over moves it to you.`,
    };
    this.say(texts[this.control]);
    this.takeOver.hidden = this.control === "you";
    this.handBack.hidden = this.control !== "you";
    this.browser.hidden = this.control !== "you";
  }

  /** The browser launcher: a browser on the screen sharing the agent's
   *  screen browser profile (sign in there once for both). */
  private async openBrowser(): Promise<void> {
    try {
      await this.client.call("screen.browser", {});
      this.say("Opened a browser on the screen; it shares the agent's browser profile.");
    } catch (err) {
      this.say((err as Error).message);
    }
  }

  private say(text: string): void {
    this.status.textContent = text;
  }

  /** Draws on the canvas. */
  private surface(): Surface {
    const canvas = this.canvas;
    const context = () => canvas.getContext("2d") as CanvasRenderingContext2D;
    return {
      resize: (width, height) => {
        canvas.width = width;
        canvas.height = height;
        canvas.dataset.size = `${width}x${height}`;
      },
      put: (x, y, width, height, rgba) => {
        const pixels = new Uint8ClampedArray(rgba);
        for (let i = 3; i < pixels.length; i += 4) pixels[i] = 255;
        context().putImageData(new ImageData(pixels, width, height), x, y);
      },
      copy: (srcX, srcY, x, y, width, height) => {
        context().drawImage(canvas, srcX, srcY, width, height, x, y, width, height);
      },
    };
  }

  /** The canvas's pointer and keys go to the screen while this window
   *  has control. */
  private input(): void {
    const canvas = this.canvas;
    const at = (event: MouseEvent): [number, number] => {
      const box = canvas.getBoundingClientRect();
      const scale = (n: number, size: number, shown: number) => (shown > 0 ? (n * size) / shown : 0);
      return [scale(event.clientX - box.left, canvas.width, box.width), scale(event.clientY - box.top, canvas.height, box.height)];
    };
    const send = (event: MouseEvent) => {
      if (this.control !== "you") return;
      const [x, y] = at(event);
      this.rfb?.pointer(x, y, this.buttons);
    };
    const bit = (button: number) => [1, 2, 4][button] ?? 0;
    canvas.addEventListener("mousedown", (event) => {
      canvas.focus();
      this.buttons |= bit(event.button);
      send(event);
    });
    canvas.addEventListener("mouseup", (event) => {
      this.buttons &= ~bit(event.button);
      send(event);
    });
    canvas.addEventListener("mousemove", send);
    canvas.addEventListener("contextmenu", (event) => event.preventDefault());
    canvas.addEventListener("wheel", (event) => {
      if (this.control !== "you") return;
      event.preventDefault();
      const [x, y] = at(event);
      const wheel = event.deltaY < 0 ? 8 : 16;
      this.rfb?.pointer(x, y, this.buttons | wheel);
      this.rfb?.pointer(x, y, this.buttons);
    });
    for (const [type, down] of [
      ["keydown", true],
      ["keyup", false],
    ] as const) {
      canvas.addEventListener(type, (event) => {
        if (this.control !== "you") return;
        const sym = keysym(event.key);
        if (sym === null) return;
        event.preventDefault();
        this.rfb?.key(sym, down);
      });
    }
  }
}
