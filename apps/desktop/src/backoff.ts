// Reconnect backoff with full jitter (spec: desktop-app, Reconnect backoff
// with full jitter): the delay is `random() * min(15000, 300 * 2^attempt)`
// ms, and a connection that closes within 5 s of opening counts as a failed
// attempt, so a flapping daemon backs off instead of being hammered.

export const BASE_MS = 300;
export const CAP_MS = 15_000;
/** A connection open at least this long was stable. */
export const STABLE_MS = 5_000;

export class Backoff {
  attempt = 0;
  private openedAt: number | null = null;

  constructor(
    private readonly random: () => number = Math.random,
    private readonly now: () => number = Date.now,
  ) {}

  /** The socket opened. */
  opened(): void {
    this.openedAt = this.now();
  }

  /** The socket closed: a failed attempt unless it was stable. */
  closed(): void {
    const stable = this.openedAt !== null && this.now() - this.openedAt >= STABLE_MS;
    this.openedAt = null;
    this.attempt = stable ? 0 : this.attempt + 1;
  }

  /** A dial failed before the socket opened. */
  failed(): void {
    this.openedAt = null;
    this.attempt += 1;
  }

  /** The upper bound of the next delay. */
  ceiling(): number {
    return Math.min(CAP_MS, BASE_MS * 2 ** this.attempt);
  }

  /** The next delay. */
  delay(): number {
    return this.random() * this.ceiling();
  }
}
