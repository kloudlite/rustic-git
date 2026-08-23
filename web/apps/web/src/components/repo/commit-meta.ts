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
 *  elapsed hours: a commit at 1am is "today" to the person who made it. */
export function dayBucket(seconds: number) {
  const at = new Date(seconds * 1000);
  const today = new Date();
  const midnight = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((midnight(today) - midnight(at)) / 86_400_000);
  if (days <= 0) return "Today";
  if (days === 1) return "Yesterday";
  return at.toLocaleDateString("en", { year: "numeric", month: "long", day: "numeric" });
}
