// The routines pane's schedule picker as a pure function (spec: bot-mode,
// Bot routines): every choice maps to a string of the `prometheus cron`
// grammar, a five-field cron expression or an `every <n>m|h` interval,
// with out-of-range input clamped instead of passed on.

/** One choice of the schedule picker. */
export type Cadence =
  | { kind: "daily"; time: string }
  | { kind: "weekdays"; time: string }
  | { kind: "weekly"; day: number; time: string }
  | { kind: "hours"; every: number }
  | { kind: "minutes"; every: number };

export const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/** `HH:MM` as cron's minute and hour fields (midnight when unreadable). */
function clock(time: string): string {
  const match = /^(\d{1,2}):(\d{2})$/.exec(time);
  const hour = Math.min(23, Number(match?.[1] ?? 0));
  const minute = Math.min(59, Number(match?.[2] ?? 0));
  return `${minute} ${hour}`;
}

/** A whole count of at least one. */
const count = (n: number) => Math.max(1, Math.floor(Number.isFinite(n) ? n : 1));

/** The schedule string a picker choice stands for. */
export function scheduleOf(cadence: Cadence): string {
  switch (cadence.kind) {
    case "daily":
      return `${clock(cadence.time)} * * *`;
    case "weekdays":
      return `${clock(cadence.time)} * * 1-5`;
    case "weekly":
      return `${clock(cadence.time)} * * ${Math.min(6, Math.max(0, Math.floor(cadence.day)))}`;
    case "hours":
      return `every ${count(cadence.every)}h`;
    default:
      return `every ${count(cadence.every)}m`;
  }
}
