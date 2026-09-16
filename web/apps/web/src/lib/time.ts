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
/** A plain calendar date. Exported so a commit list and a commit detail page cannot
 *  disagree about how a day is spelled. */
export const ABSOLUTE = new Intl.DateTimeFormat("en", { year: "numeric", month: "short", day: "numeric" });

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["day", 86_400],
  ["hour", 3_600],
  ["minute", 60],
];

/** `ms` is a unix timestamp in milliseconds. A non-finite `ms` (a null `createdAt` upstream,
 *  e.g.) has no sensible relative form — say so rather than let `Intl` throw or lie. */
export function when(ms: number): string {
  if (!Number.isFinite(ms)) return "unknown";
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
// Date and time formatted apart and joined here: one formatter given both says " at " on
// some ICU builds and ", " on others, which is the same mismatch by a different route.
const STAMP_DAY = new Intl.DateTimeFormat("en", { year: "numeric", month: "short", day: "numeric", timeZone: "UTC" });
const STAMP_TIME = new Intl.DateTimeFormat("en", { hour: "numeric", minute: "2-digit", timeZone: "UTC" });
// `Intl.DateTimeFormat.format` throws a RangeError on NaN — a null `createdAt` reaches here as
// `stamp(snapshotTime(c))` from a client component (env-snapshots.tsx), so an unguarded throw
// takes the whole page down over one missing timestamp.
export const stamp = (ms: number) => (Number.isFinite(ms) ? `${STAMP_DAY.format(ms)}, ${STAMP_TIME.format(ms)} UTC` : "unknown");

/** An incident window, in the zone the people who decide about one actually work in. Pinned to
 *  IST rather than the viewer's zone for the same hydration reason `stamp` is pinned to UTC, and
 *  the label says which zone it is so a window is never read as local by accident. */
const IST_DAY = new Intl.DateTimeFormat("en", { year: "numeric", month: "short", day: "numeric", timeZone: "Asia/Kolkata" });
const IST_TIME = new Intl.DateTimeFormat("en", { hour: "2-digit", minute: "2-digit", hour12: false, timeZone: "Asia/Kolkata" });
export const ist = (ms: number) => (Number.isFinite(ms) ? `${IST_DAY.format(ms)}, ${IST_TIME.format(ms)} IST` : "unknown");

/** The same instant as a `datetime-local` value (`YYYY-MM-DDTHH:mm`) in IST, so a prefilled form
 *  and the table above it read the same. The form states the zone; `exclusionPayload` parses it
 *  back with an explicit `+05:30`, so a browser in another zone still means the same window. */
export function istInput(ms: number): string {
  if (!Number.isFinite(ms)) return "";
  const p = new Intl.DateTimeFormat("en-CA", {
    timeZone: "Asia/Kolkata",
    year: "numeric", month: "2-digit", day: "2-digit",
    hour: "2-digit", minute: "2-digit", hour12: false,
  }).formatToParts(ms);
  const at = (t: string) => p.find((x) => x.type === t)?.value ?? "00";
  return `${at("year")}-${at("month")}-${at("day")}T${at("hour")}:${at("minute")}`;
}

/** The same, for the unix SECONDS that git objects carry. */
export const whenSeconds = (seconds: number) => when(seconds * 1000);

/** A file size a person reads. `null` is "not a blob", which has no size.
 *  Volumes report GB-scale numbers (`Volume.status.usedBytes`), so the ladder does not stop at
 *  MB — "48000.0 MB" is a number nobody reads. */
export function size(bytes: number | null): string {
  if (bytes === null) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} GB`;
}
