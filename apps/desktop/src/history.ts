// Composer history (spec: desktop-app, Composer history and queue
// editing): in an empty composer `Up` and `Down` recall the profile's
// previous prompts, newest first.

const LIMIT = 200;

export class PromptHistory {
  private cursor = -1;

  constructor(private readonly profile: () => string) {}

  private key(): string {
    return `prometheus.history.${this.profile()}`;
  }

  private list(): string[] {
    try {
      return (JSON.parse(localStorage.getItem(this.key()) ?? "[]") as string[]).filter((s) => typeof s === "string");
    } catch {
      return [];
    }
  }

  /** A prompt was sent. */
  push(text: string): void {
    const list = [text, ...this.list().filter((t) => t !== text)].slice(0, LIMIT);
    try {
      localStorage.setItem(this.key(), JSON.stringify(list));
    } catch {
      // History is a convenience.
    }
    this.reset();
  }

  reset(): void {
    this.cursor = -1;
  }

  /** The older (`up`) or newer prompt; `""` past the newest. */
  step(up: boolean): string | null {
    const list = this.list();
    const next = this.cursor + (up ? 1 : -1);
    if (next >= list.length) return null;
    this.cursor = Math.max(-1, next);
    return this.cursor === -1 ? "" : (list[this.cursor] ?? "");
  }
}
