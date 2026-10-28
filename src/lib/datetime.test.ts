import { describe, expect, it } from "vitest";

import { formatLocalDate, formatLocalDateTime } from "./datetime";

/**
 * Invoices store RFC3339 UTC instants, and the user locates a mail by the clock
 * in their own mailbox, so rendering must go through the local time zone.
 */
describe("local time formatting", () => {
  const instant = "2026-09-10T06:35:00Z";

  it("renders an instant with the machine's own offset", () => {
    const date = new Date(instant);
    const pad = (value: number) => String(value).padStart(2, "0");
    const expected =
      `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
      ` ${pad(date.getHours())}:${pad(date.getMinutes())}`;

    expect(formatLocalDateTime(instant)).toBe(expected);
  });

  it("does not simply echo the stored UTC hour outside UTC", () => {
    const date = new Date(instant);
    if (date.getTimezoneOffset() === 0) {
      // A UTC machine cannot tell the two apart; the offset test above still
      // holds, and CI runs in UTC.
      expect(formatLocalDateTime(instant)).toBe("2026-09-10 06:35");
      return;
    }
    expect(formatLocalDateTime(instant)).not.toBe("2026-09-10 06:35");
  });

  it("keeps the calendar day in local time and survives unusable input", () => {
    expect(formatLocalDate(instant)).toMatch(/^2026-09-(09|10)$/);
    expect(formatLocalDateTime("not a date")).toBe("not a date");
  });
});
