// A minimal RFB 3.8 viewer protocol (RFC 6143) for the bot screen
// (spec: execution-environments, Bot screen): the daemon's `/screen`
// stream relays Xvnc's RFB socket, which offers only security type None
// (the display ticket is the admission). The client asks for 32-bit
// true colour laid out as canvas RGBA bytes and the Raw and CopyRect
// encodings plus the DesktopSize pseudo-encoding; that is all a local
// TigerVNC needs. Bytes in, drawing and messages out: no DOM here.

/** What the viewer draws on. */
export interface Surface {
  resize(width: number, height: number): void;
  /** `rgba`: width * height * 4 bytes, alpha unset. */
  put(x: number, y: number, width: number, height: number, rgba: Uint8Array): void;
  copy(srcX: number, srcY: number, x: number, y: number, width: number, height: number): void;
}

const VERSION = "RFB 003.008\n";
const SECURITY_NONE = 1;
const ENCODINGS = [0 /* Raw */, 1 /* CopyRect */, -223 /* DesktopSize */];

type Phase = "version" | "security" | "result" | "init" | "normal" | "failed";

/** Thrown when the stream is not an RFB server this client can use. */
export class RfbError extends Error {}

export class RfbClient {
  private phase: Phase = "version";
  private buffer = new Uint8Array(0);
  width = 0;
  height = 0;
  name = "";
  /** Bytes to send to the server. */
  onSend: (bytes: Uint8Array) => void = () => {};
  /** The handshake completed; the screen is `width` x `height`. */
  onReady: () => void = () => {};

  constructor(private readonly surface: Surface) {}

  /** Feeds bytes from the server. */
  receive(bytes: Uint8Array): void {
    const joined = new Uint8Array(this.buffer.length + bytes.length);
    joined.set(this.buffer);
    joined.set(bytes, this.buffer.length);
    this.buffer = joined;
    let used = this.step();
    while (used > 0) {
      this.buffer = this.buffer.subarray(used);
      used = this.step();
    }
  }

  /** One message from the buffer: the bytes it used, 0 while incomplete. */
  private step(): number {
    switch (this.phase) {
      case "version":
        return this.version();
      case "security":
        return this.security();
      case "result":
        return this.result();
      case "init":
        return this.serverInit();
      case "normal":
        return this.message();
      default:
        return 0;
    }
  }

  private fail(why: string): never {
    this.phase = "failed";
    throw new RfbError(why);
  }

  private version(): number {
    if (this.buffer.length < 12) return 0;
    const greeting = new TextDecoder().decode(this.buffer.subarray(0, 12));
    if (!greeting.startsWith("RFB 003.")) this.fail(`not an RFB server: ${JSON.stringify(greeting)}`);
    this.onSend(new TextEncoder().encode(VERSION));
    this.phase = "security";
    return 12;
  }

  private security(): number {
    const b = this.buffer;
    if (b.length < 1) return 0;
    const count = b[0] ?? 0;
    if (count === 0) return this.reason(1, "the server refused the connection");
    if (b.length < 1 + count) return 0;
    if (!b.subarray(1, 1 + count).includes(SECURITY_NONE)) this.fail("the screen asks for a password this viewer does not have");
    this.onSend(Uint8Array.of(SECURITY_NONE));
    this.phase = "result";
    return 1 + count;
  }

  private result(): number {
    if (this.buffer.length < 4) return 0;
    if (u32(this.buffer, 0) !== 0) return this.reason(4, "the screen refused the viewer");
    this.onSend(Uint8Array.of(1)); // ClientInit: shared
    this.phase = "init";
    return 4;
  }

  /** A failure reason string at `at`; fails with it once complete. */
  private reason(at: number, fallback: string): number {
    if (this.buffer.length < at + 4) return 0;
    const length = u32(this.buffer, at);
    if (this.buffer.length < at + 4 + length) return 0;
    const text = new TextDecoder().decode(this.buffer.subarray(at + 4, at + 4 + length));
    return this.fail(text || fallback);
  }

  private serverInit(): number {
    const b = this.buffer;
    if (b.length < 24) return 0;
    const nameLength = u32(b, 20);
    if (b.length < 24 + nameLength) return 0;
    this.width = u16(b, 0);
    this.height = u16(b, 2);
    this.name = new TextDecoder().decode(b.subarray(24, 24 + nameLength));
    this.surface.resize(this.width, this.height);
    this.onSend(pixelFormat());
    this.onSend(encodings());
    this.phase = "normal";
    this.request(false);
    this.onReady();
    return 24 + nameLength;
  }

  /** Asks for the whole screen (`incremental`: only what changed). */
  request(incremental: boolean): void {
    const m = new DataView(new ArrayBuffer(10));
    m.setUint8(0, 3);
    m.setUint8(1, incremental ? 1 : 0);
    m.setUint16(6, this.width);
    m.setUint16(8, this.height);
    this.onSend(new Uint8Array(m.buffer));
  }

  private message(): number {
    const b = this.buffer;
    if (b.length < 1) return 0;
    switch (b[0]) {
      case 0:
        return this.update();
      case 1: {
        // SetColourMapEntries: never sent for true colour, skipped.
        if (b.length < 6) return 0;
        const length = 6 + 6 * u16(b, 4);
        return b.length < length ? 0 : length;
      }
      case 2:
        return 1; // Bell
      case 3: {
        if (b.length < 8) return 0;
        const length = 8 + u32(b, 4);
        return b.length < length ? 0 : length;
      }
      default:
        return this.fail(`unknown server message ${b[0]}`);
    }
  }

  /** A whole FramebufferUpdate, applied once every rectangle arrived. */
  private update(): number {
    const b = this.buffer;
    if (b.length < 4) return 0;
    const count = u16(b, 2);
    let at = 4;
    const rects: [number, number, number, number, number, number][] = [];
    for (let i = 0; i < count; i++) {
      if (b.length < at + 12) return 0;
      const [x, y, w, h] = [u16(b, at), u16(b, at + 2), u16(b, at + 4), u16(b, at + 6)];
      const encoding = new DataView(b.buffer, b.byteOffset + at + 8, 4).getInt32(0);
      const size = encoding === 0 ? w * h * 4 : encoding === 1 ? 4 : encoding === -223 ? 0 : this.fail(`unexpected encoding ${encoding}`);
      if (b.length < at + 12 + size) return 0;
      rects.push([x, y, w, h, encoding, at + 12]);
      at += 12 + size;
    }
    for (const [x, y, w, h, encoding, data] of rects) this.apply(x, y, w, h, encoding, data);
    this.request(true);
    return at;
  }

  private apply(x: number, y: number, w: number, h: number, encoding: number, data: number): void {
    const b = this.buffer;
    if (encoding === 0) this.surface.put(x, y, w, h, b.slice(data, data + w * h * 4));
    else if (encoding === 1) this.surface.copy(u16(b, data), u16(b, data + 2), x, y, w, h);
    else {
      this.width = w;
      this.height = h;
      this.surface.resize(w, h);
    }
  }

  /** A pointer at (`x`, `y`) with `buttons` (bit 0 left, 1 middle, 2
   *  right, 3/4 wheel up/down). */
  pointer(x: number, y: number, buttons: number): void {
    if (this.phase !== "normal") return;
    const m = new DataView(new ArrayBuffer(6));
    m.setUint8(0, 5);
    m.setUint8(1, buttons);
    m.setUint16(2, clamp(x, this.width));
    m.setUint16(4, clamp(y, this.height));
    this.onSend(new Uint8Array(m.buffer));
  }

  /** An X keysym pressed or released. */
  key(keysym: number, down: boolean): void {
    if (this.phase !== "normal") return;
    const m = new DataView(new ArrayBuffer(8));
    m.setUint8(0, 4);
    m.setUint8(1, down ? 1 : 0);
    m.setUint32(4, keysym);
    this.onSend(new Uint8Array(m.buffer));
  }
}

function u16(b: Uint8Array, at: number): number {
  return ((b[at] ?? 0) << 8) | (b[at + 1] ?? 0);
}

function u32(b: Uint8Array, at: number): number {
  return new DataView(b.buffer, b.byteOffset + at, 4).getUint32(0);
}

function clamp(n: number, size: number): number {
  return Math.min(Math.max(0, Math.round(n)), Math.max(0, size - 1));
}

/** SetPixelFormat: 32 bpp little-endian true colour, red in the lowest
 *  byte, so each pixel's bytes are canvas R, G, B. */
function pixelFormat(): Uint8Array {
  const m = new DataView(new ArrayBuffer(20));
  m.setUint8(0, 0);
  m.setUint8(4, 32); // bits per pixel
  m.setUint8(5, 24); // depth
  m.setUint8(6, 0); // little-endian
  m.setUint8(7, 1); // true colour
  for (const at of [8, 10, 12]) m.setUint16(at, 255);
  m.setUint8(14, 0); // red shift
  m.setUint8(15, 8); // green shift
  m.setUint8(16, 16); // blue shift
  return new Uint8Array(m.buffer);
}

function encodings(): Uint8Array {
  const m = new DataView(new ArrayBuffer(4 + 4 * ENCODINGS.length));
  m.setUint8(0, 2);
  m.setUint16(2, ENCODINGS.length);
  ENCODINGS.forEach((encoding, i) => m.setInt32(4 + 4 * i, encoding));
  return new Uint8Array(m.buffer);
}

/** Named keys' X keysyms. */
const KEYSYMS: Record<string, number> = {
  Backspace: 0xff08,
  Tab: 0xff09,
  Enter: 0xff0d,
  Escape: 0xff1b,
  Delete: 0xffff,
  Home: 0xff50,
  ArrowLeft: 0xff51,
  ArrowUp: 0xff52,
  ArrowRight: 0xff53,
  ArrowDown: 0xff54,
  PageUp: 0xff55,
  PageDown: 0xff56,
  End: 0xff57,
  Insert: 0xff63,
  Shift: 0xffe1,
  Control: 0xffe3,
  Alt: 0xffe9,
  Meta: 0xffe7,
  CapsLock: 0xffe5,
  F1: 0xffbe,
  F2: 0xffbf,
  F3: 0xffc0,
  F4: 0xffc1,
  F5: 0xffc2,
  F6: 0xffc3,
  F7: 0xffc4,
  F8: 0xffc5,
  F9: 0xffc6,
  F10: 0xffc7,
  F11: 0xffc8,
  F12: 0xffc9,
};

/** The X keysym of a DOM `key`: a named key, a Latin-1 character as
 *  itself, any other character as `0x01000000 + code point`. */
export function keysym(key: string): number | null {
  const named = KEYSYMS[key];
  if (named !== undefined) return named;
  const chars = [...key];
  if (chars.length !== 1) return null;
  const code = key.codePointAt(0) ?? 0;
  return code < 0x100 ? code : 0x01000000 + code;
}
