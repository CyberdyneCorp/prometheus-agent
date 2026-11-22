// The one activity-to-state mapping (spec: pets, One activity-to-state
// mapping), mirrored from `pmt_pets::state`: the same signals and the same
// precedence as the line interface and the TUI.

export type PetState = "idle" | "wave" | "run" | "failed" | "review" | "jump" | "waiting";

/** How long a momentary state (failed, jump, wave, the heart) shows. */
export const MOMENT_MS = 3000;

export interface Activity {
  failed: boolean;
  celebrate: boolean;
  awaitingUser: boolean;
  toolRunning: boolean;
  reasoning: boolean;
  busy: boolean;
  justFinished: boolean;
}

/** The state of `a`, in the specified precedence. */
export function derive(a: Activity): PetState {
  const ranked: [boolean, PetState][] = [
    [a.failed, "failed"],
    [a.celebrate, "jump"],
    [a.awaitingUser, "waiting"],
    [a.toolRunning, "run"],
    [a.reasoning, "review"],
    [a.busy, "run"],
    [a.justFinished, "wave"],
  ];
  return ranked.find(([on]) => on)?.[1] ?? "idle";
}

/** The speech bubble of a state on the pop-out pet. */
export function bubble(state: PetState): string {
  return state === "run" ? "working…" : state === "review" ? "thinking…" : state === "waiting" ? "your turn" : "";
}

function todosDone(output: unknown): boolean {
  let reply = output;
  if (typeof output === "string") {
    try {
      reply = JSON.parse(output) as unknown;
    } catch {
      return false;
    }
  }
  const todos = (reply as { todos?: { status?: string }[] } | null)?.todos;
  return Array.isArray(todos) && todos.length > 0 && todos.every((t) => t.status === "completed" || t.status === "cancelled");
}

/** Follows the shown session's events. */
export class Tracker {
  private tools = 0;
  private reasoning = false;
  private busy = false;
  private waiting = 0;
  private failedAt = -Infinity;
  private celebratedAt = -Infinity;
  private finishedAt = -Infinity;
  private heartAt = -Infinity;

  /** A trajectory event (`session.event`'s `type` and `payload`). */
  event(kind: string, payload: Record<string, unknown>, now = Date.now()): void {
    switch (kind) {
      case "turn.start":
        this.busy = true;
        this.finishedAt = -Infinity;
        break;
      case "assistant.reasoning":
        this.reasoning = true;
        break;
      case "tool.call":
        this.reasoning = false;
        this.tools += 1;
        break;
      case "tool.result":
        this.tools = Math.max(0, this.tools - 1);
        if (payload.ok === false) this.failedAt = now;
        if (payload.tool === "todo" && todosDone(payload.output)) this.celebratedAt = now;
        break;
      case "approval.request":
      case "clarify.request":
        this.waiting += 1;
        break;
      case "approval.decision":
      case "clarify.answer":
        this.waiting = Math.max(0, this.waiting - 1);
        break;
      case "error":
        this.failedAt = now;
        break;
      case "turn.end":
        this.turnEnded(payload.reason === "error", now);
        break;
    }
  }

  turnEnded(failed: boolean, now = Date.now()): void {
    this.busy = false;
    this.reasoning = false;
    this.tools = 0;
    this.waiting = 0;
    if (failed) this.failedAt = now;
    else this.finishedAt = now;
  }

  celebrate(now = Date.now()): void {
    this.celebratedAt = now;
  }

  heart(now = Date.now()): void {
    this.heartAt = now;
  }

  hearts(now = Date.now()): boolean {
    return now - this.heartAt < MOMENT_MS;
  }

  idle(now = Date.now()): boolean {
    return this.state(now) === "idle";
  }

  state(now = Date.now()): PetState {
    const recent = (at: number) => now - at < MOMENT_MS;
    return derive({
      failed: recent(this.failedAt),
      celebrate: recent(this.celebratedAt),
      awaitingUser: this.waiting > 0,
      toolRunning: this.tools > 0,
      reasoning: this.reasoning,
      busy: this.busy,
      justFinished: recent(this.finishedAt),
    });
  }
}

/** Affection or gratitude aimed at the agent (the heart reaction), from
 *  the same fixed lexicon as `pmt_pets::affection`. */
const PHRASES = ["good bot", "good boy", "good girl", "good job", "nice work", "well done", "great job", "thank you", "thank u", "thanks", "thx", "ty", "love you", "love u", "ily", "you rock", "you're the best", "youre the best", "you are the best", "obrigado", "obrigada", "gracias", "merci", "danke"];
const SYMBOLS = ["<3", "❤", "♥", "\u{1f496}", "\u{1f495}", "\u{1f497}", "\u{1f49c}", "\u{1f499}", "\u{1f49a}", "\u{1f49b}", "\u{1f9e1}", "\u{1f970}", "\u{1f60d}", "\u{1f618}", "\u{1f64f}"];

export function isAffectionate(text: string): boolean {
  if (SYMBOLS.some((s) => text.includes(s))) return true;
  const words = text.toLowerCase().split(/[^\p{L}\p{N}']+/u).filter(Boolean);
  return PHRASES.some((phrase) => {
    const wanted = phrase.split(" ");
    return words.some((_, i) => wanted.every((w, j) => words[i + j] === w));
  });
}
