// Composer drafts (spec: desktop-app, Drafts are durable and never sent
// for the user): each chat of each window keeps its text and staged
// images in the window's storage, so they survive a relaunch; an image
// message the daemon never acknowledged is offered back once. Nothing
// here submits, navigates or focuses.

/** An image staged in the composer: a file the daemon reads when the
 *  message is sent (picked with the OS file picker), or a snapshot the
 *  window made (a `data:` URL, an annotation's). */
export interface StagedImage {
  name: string;
  path?: string;
  data?: string;
}

export interface Draft {
  text: string;
  images: StagedImage[];
}

/** The chat a draft belongs to before it has a session. */
export const NEW_CHAT = "new";

function read<T>(key: string): T | null {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

function write(key: string, value: unknown): void {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Storage full or unavailable: the draft lives until the window closes.
  }
}

export class Drafts {
  constructor(private readonly window: string) {}

  private key(kind: string, chat: string): string {
    return `prometheus.${kind}.${this.window}.${chat}`;
  }

  get(chat: string): Draft | null {
    return read<Draft>(this.key("draft", chat));
  }

  /** Saves `draft` for `chat`; an empty one is removed. */
  set(chat: string, draft: Draft): void {
    const empty = draft.text === "" && draft.images.length === 0;
    write(this.key("draft", chat), empty ? null : draft);
  }

  /** Takes a chat's draft away (its session is gone). */
  take(chat: string): Draft | null {
    const draft = this.get(chat);
    write(this.key("draft", chat), null);
    return draft;
  }

  /** An image message is being sent: kept until the daemon admits it. */
  sending(chat: string, draft: Draft): void {
    if (draft.images.length > 0) write(this.key("unacknowledged", chat), draft);
  }

  /** The daemon admitted the chat's message. */
  acknowledged(chat: string): void {
    write(this.key("unacknowledged", chat), null);
  }

  /** The image message of `chat` that was never acknowledged, offered
   *  once: taking it forgets it. */
  takeUnacknowledged(chat: string): Draft | null {
    const draft = read<Draft>(this.key("unacknowledged", chat));
    write(this.key("unacknowledged", chat), null);
    return draft;
  }
}
