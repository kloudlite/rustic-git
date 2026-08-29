/** Shared commit formatting, so a list and a detail page never disagree about
 *  what "yesterday" means or where a message stops being a title. */

export function commitTitle(message: string) {
  return message.split("\n")[0];
}

/** Everything after the first blank line — the part git calls the body. */
export function commitBody(message: string) {
  const rest = message.split("\n").slice(1).join("\n").trim();
  return rest || undefined;
}

/** Which heading a commit sits under in a history list. Calendar days, not
 *  elapsed hours: a commit at 1am is "today" to the person who made it.
 *
 *  UTC days, explicitly: this runs on the server, whose zone is the pod's and not the
 *  viewer's, so the pod's midnight was never the viewer's either. Pinning it makes the
 *  grouping the same from every replica and every laptop.
 *  ponytail: the viewer's zone would need a client component or a cookie; do that if
 *  "Yesterday" at 1am local becomes a complaint. */
export function dayBucket(seconds: number, now = Date.now()) {
  const at = new Date(seconds * 1000);
  const days = Math.round((utcMidnight(new Date(now)) - utcMidnight(at)) / 86_400_000);
  if (days <= 0) return "Today";
  if (days === 1) return "Yesterday";
  return HEADING.format(at);
}

// Not `lib/time`'s ABSOLUTE: a history heading spells the month out and is pinned to UTC to
// match the bucketing above. Built once, not once per commit.
const HEADING = new Intl.DateTimeFormat("en", { year: "numeric", month: "long", day: "numeric", timeZone: "UTC" });

const utcMidnight = (d: Date) => Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate());
