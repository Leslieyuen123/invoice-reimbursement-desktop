/**
 * Local-time formatting for invoice timestamps.
 *
 * Invoices are stored as RFC3339 UTC instants. Locating the mail that carried
 * an invoice means comparing against the clock in the mailbox, so every date
 * the user reads must be rendered in the machine's local time instead of the
 * stored UTC hour.
 */

function parse(value: string): Date | null {
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

function pad(value: number, width = 2) {
  return String(value).padStart(width, "0");
}

/** `2026-09-10 14:32` in local time, or the raw value when unparsable. */
export function formatLocalDateTime(value: string): string {
  const date = parse(value);
  if (!date) return value;
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
    ` ${pad(date.getHours())}:${pad(date.getMinutes())}`
  );
}

/** `2026-09-10` in local time, or the raw value when unparsable. */
export function formatLocalDate(value: string): string {
  const date = parse(value);
  if (!date) return value;
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}
