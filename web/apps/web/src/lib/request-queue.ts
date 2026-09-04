import type { OwnerRow } from "@/lib/api";
import type { RequestDoc, RequestKind } from "@/lib/requests";
import { DIMS, dimLabel, dimUnit, type QuotaDim } from "@/lib/quota";

export type QueueFilter = { kind: RequestKind | "any"; ownerType: "any" | "person" | "team"; age: "any" | "1d" | "7d" };

const AGE_MS: Record<"1d" | "7d", number> = { "1d": 86_400_000, "7d": 7 * 86_400_000 };

/** Client-side: the fleet-wide queue is a few dozen rows, so one fetch feeds both tabs, all three
 *  filters and each row's own owner history. `age` means "older than", which is the question an
 *  operator actually asks of a queue.
 *
 *  `teams` is the owners list's own `isTeam` — a `RequestDoc` carries no owner kind, and guessing
 *  from the slug would file a person named like a team under the wrong filter. Without it (the
 *  owners read degraded) the owner-type filter cannot answer, so it keeps every row rather than
 *  silently emptying the queue. */
export function filterQueue(rows: RequestDoc[], f: QueueFilter, now: number, teams?: ReadonlySet<string>): RequestDoc[] {
  return rows.filter((r) => {
    if (f.kind !== "any" && r.kind !== f.kind) return false;
    if (f.ownerType !== "any" && teams && teams.has(r.owner) !== (f.ownerType === "team")) return false;
    if (f.age !== "any" && now - new Date(r.createdAt ?? 0).getTime() < AGE_MS[f.age]) return false;
    return true;
  });
}

function firstDim(r: RequestDoc): QuotaDim | null {
  return DIMS.find((d) => r.quota?.[d] !== undefined) ?? null;
}

/** The first line of a queue row. One sentence per kind — it ellipses rather than wrapping, so the
 *  row height stays fixed and the table never reflows under the poll. */
export function summaryLine(r: RequestDoc): string {
  if (r.kind === "quota") {
    const d = firstDim(r);
    return d ? `Raise ${dimLabel(d).toLowerCase()} to ${r.quota?.[d]}` : "Raise a quota";
  }
  if (r.kind === "access") return `Become ${r.access?.role ?? "a member"} on ${r.access?.team ?? "a team"}`;
  if (r.kind === "region") return `Enable ${r.region?.region ?? "a region"}`;
  return r.other?.title ?? "Request";
}

/** The muted second line: the kind-specific FACT the decider needs before opening anything.
 *
 *  Only quota has a real one on the wire — `OwnerRow` carries usage. The api answers no current
 *  role for an access request and no region list for an owner, so those lines restate the ask
 *  rather than inventing a fact; the panel's Facts block says the same and no more. */
export function contextLine(r: RequestDoc, usage: OwnerRow | undefined): string {
  if (r.kind === "quota") {
    const d = firstDim(r);
    if (!d) return "no dimension named";
    if (!usage) return "current usage unavailable";
    const unit = dimUnit(d);
    return `${usage.used[d]} / ${usage.limit[d]}${unit ? ` ${unit}` : ""} in use`;
  }
  if (r.kind === "access") return r.access?.team ? `on team ${r.access.team}` : "no team named";
  if (r.kind === "region") return "current regions unavailable";
  return (r.other?.body ?? "").split("\n")[0];
}
