/**
 * Relative time, in one place.
 *
 * Three copies of this existed — two byte-identical, one differing only in unit —
 * and each hand-rolled the plural forms the platform already knows.
 * `Intl.RelativeTimeFormat` is that platform feature.
 *
 * The locale is pinned rather than left to the environment. These render on the
 * server and hydrate in the browser, and if the two disagree about how to spell
 * "2 days ago" React reports a hydration mismatch — a bug that only ever appears
 * on someone else's machine.
 */
const RELATIVE = new Intl.RelativeTimeFormat("en", { numeric: "auto" });
const ABSOLUTE = new Intl.DateTimeFormat("en", { year: "numeric", month: "short", day: "numeric" });

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["day", 86_400],
  ["hour", 3_600],
  ["minute", 60],
];

/** `ms` is a unix timestamp in milliseconds. */
export function when(ms: number): string {
  const seconds = Math.round((ms - Date.now()) / 1000);
  const ago = Math.abs(seconds);
  if (ago < 45) return "just now";
  // Past a month, a date is more use than a count of days.
  if (ago >= 2_592_000) return ABSOLUTE.format(ms);
  for (const [unit, size] of UNITS) {
    if (ago >= size) return RELATIVE.format(Math.round(seconds / size), unit);
  }
  return "just now";
}

/** The absolute instant behind a relative one, for a `title`. UTC, pinned, because this too
 *  renders on the server and hydrates in the browser — `toLocaleString()` in the pod's zone and
 *  again in the viewer's is an attribute mismatch on every row. */
const STAMP = new Intl.DateTimeFormat("en", { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" });
export const stamp = (ms: number) => `${STAMP.format(ms)} UTC`;

/** The same, for the unix SECONDS that git objects carry. */
export const whenSeconds = (seconds: number) => when(seconds * 1000);

/** A file size a person reads. `null` is "not a blob", which has no size. */
export function size(bytes: number | null): string {
  if (bytes === null) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
