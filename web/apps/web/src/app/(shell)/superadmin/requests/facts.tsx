import type { OwnerRow, RequestDoc } from "@/lib/api";
import { DIMS, dimLabel, dimUnit, type QuotaDim } from "@/lib/quota";
import { CapacityBar } from "../ui/capacity-bar";

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3 border-b border-border py-1.5 last:border-0">
      <span className="text-caption text-muted-foreground">{k}</span>
      <span className="text-sm2 tabular-nums">{v}</span>
    </div>
  );
}

/** What a decider needs BEFORE the buttons, per kind. Kept out of the panel so the panel is one
 *  form with one submit path — the four kinds differ in what they show, never in how they decide.
 *
 *  Only quota has facts beyond the request itself: `OwnerRow` carries usage. The api answers no
 *  current role and no per-owner region list, so those blocks restate what was asked rather than
 *  claiming a fact nobody sent. */
export function Facts({ request, usage }: { request: RequestDoc; usage: OwnerRow | undefined }) {
  const owner = <Row k="Owner" v={usage?.isTeam ? `${request.owner} (team)` : request.owner} />;

  if (request.kind === "quota") {
    const d = DIMS.find((x) => request.quota?.[x] !== undefined) as QuotaDim | undefined;
    if (!d) return <Row k="Requested" v="no dimension named" />;
    const unit = dimUnit(d);
    return (
      <div className="flex flex-col gap-3">
        <div>
          {owner}
          <Row k="Dimension" v={dimLabel(d)} />
          <Row k="In use" v={usage ? String(usage.used[d]) : "unavailable"} />
          <Row k="Current limit" v={usage ? `${usage.limit[d]}${unit ? ` ${unit}` : ""} (${usage.source} quota)` : "unavailable"} />
          <Row k="Requested" v={`${request.quota?.[d]}${unit ? ` ${unit}` : ""}`} />
        </div>
        {usage && <CapacityBar used={usage.used[d]} limit={usage.limit[d]} unit={unit || dimLabel(d).toLowerCase()} />}
      </div>
    );
  }
  if (request.kind === "access") {
    return (
      <div>
        {owner}
        <Row k="Asker" v={request.requestedBy} />
        <Row k="Team" v={request.access?.team ?? "—"} />
        <Row k="Requested role" v={request.access?.role ?? "—"} />
      </div>
    );
  }
  if (request.kind === "region") {
    return (
      <div>
        {owner}
        <Row k="Asker" v={request.requestedBy} />
        <Row k="Region asked for" v={request.region?.region ?? "—"} />
      </div>
    );
  }
  return (
    <div className="flex flex-col gap-2">
      {owner}
      <p className="text-caption font-medium">{request.other?.title}</p>
      <p className="text-sm2 whitespace-pre-wrap text-muted-foreground">{request.other?.body}</p>
    </div>
  );
}
