"use client";

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import type { OwnerRow, RequestDoc } from "@/lib/api";
import { DIMS, dimLabel, type QuotaDim } from "@/lib/quota";
import { grantValue, summaryLine } from "@/lib/request-queue";
import { when } from "@/lib/time";
import { decideRequestAction, type DecidePayload } from "../actions";
import { Section } from "../ui/section";
import { Pill } from "../ui/pill";
import { EmptyState } from "../ui/data-table";
import { Facts } from "./facts";

/** What Approve actually does, said before the button rather than discovered after it. */
function consequence(r: RequestDoc): string {
  if (r.kind === "quota") return `Writes Quota/${r.owner}, then marks the request approved. The owner is notified.`;
  if (r.kind === "access") return `Grants ${r.access?.role ?? "the role"} on ${r.access?.team ?? "the team"}, then marks the request approved.`;
  if (r.kind === "region") return `Enables ${r.region?.region ?? "the region"} for ${r.owner}, then marks the request approved.`;
  return "Records the resolution and marks the request approved. Nothing else is written.";
}

/** One panel, four kinds. Approve carries the kind's own input — the edited grant for quota, a
 *  confirmation for access and region (the api's decide body carries neither a role nor a region,
 *  so there is nothing to edit there), and a required free-text resolution for other. Deny carries
 *  only the note, which the api requires. A 409 (someone else decided first), a 422 and a 501 land
 *  INLINE here, because the answer is only meaningful next to the request you were about to act on. */
export function DecisionPanel({
  request,
  usage,
  history,
  denyIntent,
  onDone,
}: {
  request: RequestDoc | null;
  usage: OwnerRow | undefined;
  history: RequestDoc[];
  /** The row's Deny was clicked rather than its Open: the note is what stands between here and a
   *  denial, so the panel opens with the cursor already in it. */
  denyIntent: boolean;
  onDone: () => void;
}) {
  const [grant, setGrant] = useState<Record<string, string>>({});
  const [resolution, setResolution] = useState("");
  const [confirmed, setConfirmed] = useState(false);
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  if (!request) {
    return (
      <Section eyebrow="Decision" title="Nothing selected">
        <EmptyState>Open a request to see its facts and decide it.</EmptyState>
      </Section>
    );
  }

  const req = request;
  const dims = DIMS.filter((d) => req.quota?.[d] !== undefined);
  const needsConfirm = req.kind === "access" || req.kind === "region";
  const canApprove =
    req.state === "pending" &&
    !busy &&
    (req.kind !== "other" || resolution.trim() !== "") &&
    (!needsConfirm || confirmed);

  function payload(): DecidePayload {
    if (req.kind === "quota") {
      const quota: Partial<Record<QuotaDim, number>> = {};
      for (const d of dims) quota[d] = grantValue(grant[d], req.quota?.[d]);
      return { quota };
    }
    if (req.kind === "other") return { resolution: resolution.trim() };
    return {};
  }

  async function decide(decision: "approve" | "deny") {
    setBusy(true);
    setError(null);
    const r = await decideRequestAction(req.id, decision, note.trim(), decision === "deny" ? {} : payload());
    setBusy(false);
    if (!r.ok) {
      setError(r.message);
      return;
    }
    setNote("");
    setGrant({});
    setResolution("");
    setConfirmed(false);
    onDone();
  }

  return (
    <Section
      eyebrow="Decision"
      title={`${req.owner} · ${summaryLine(req)}`}
      toolbar={<Pill tone="info">{req.kind}</Pill>}
    >
      <div className="flex flex-col gap-4">
        <Facts request={req} usage={usage} />

        <div>
          <p className="text-micro font-medium tracking-eyebrow text-muted-foreground uppercase">
            Requester note · {req.requestedBy}, {when(new Date(req.createdAt ?? 0).getTime())}
          </p>
          <p className="text-sm2 whitespace-pre-wrap">{req.reason}</p>
        </div>

        <div>
          <p className="text-micro font-medium tracking-eyebrow text-muted-foreground uppercase">
            Last 3 decisions for {req.owner}
          </p>
          {history.length === 0 ? (
            <p className="text-caption text-muted-foreground">No earlier decision for this owner.</p>
          ) : (
            <ul className="flex flex-col">
              {history.map((h) => (
                <li key={h.id} className="flex items-center gap-2 border-b border-border py-1.5 text-caption last:border-0">
                  <Pill tone={h.state === "approved" ? "ok" : "critical"}>{h.state}</Pill>
                  <span className="min-w-0 flex-1 truncate">{summaryLine(h)}</span>
                  <span className="tabular-nums text-muted-foreground">
                    {when(new Date(h.decidedAt ?? h.createdAt ?? 0).getTime())}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>

        {req.state === "pending" ? (
          <div className="flex flex-col gap-2 border-t border-border pt-4">
            {req.kind === "quota" &&
              dims.map((d) => (
                <label key={d} className="flex items-center gap-2 text-sm2">
                  <span className="w-28 shrink-0 text-muted-foreground">{dimLabel(d)}</span>
                  <Input
                    className="h-8 w-24 tabular-nums"
                    inputMode="numeric"
                    value={grant[d] ?? String(req.quota?.[d] ?? "")}
                    onChange={(e) => setGrant({ ...grant, [d]: e.target.value })}
                    aria-label={`Grant for ${dimLabel(d)}`}
                  />
                  <span className="text-caption text-muted-foreground">was {usage ? usage.limit[d] : "—"}</span>
                </label>
              ))}
            {needsConfirm && (
              <label className="flex items-start gap-2 text-sm2">
                <input type="checkbox" checked={confirmed} onChange={(e) => setConfirmed(e.target.checked)} className="mt-1" />
                <span className="text-muted-foreground">
                  {req.kind === "access"
                    ? `Grant ${req.access?.role ?? "the role"} on ${req.access?.team ?? "the team"}.`
                    : `Enable ${req.region?.region ?? "the region"} for ${req.owner}.`}
                </span>
              </label>
            )}
            {req.kind === "other" && (
              <label className="flex flex-col gap-1 text-sm2">
                <span className="text-muted-foreground">Resolution — what approving this did</span>
                <Textarea rows={2} value={resolution} onChange={(e) => setResolution(e.target.value)} className="text-sm2" />
              </label>
            )}
            <label className="flex flex-col gap-1 text-sm2">
              <span className="text-muted-foreground">Note</span>
              <Textarea
                rows={3}
                autoFocus={denyIntent}
                placeholder="Required to deny — the owner sees this"
                value={note}
                onChange={(e) => setNote(e.target.value)}
                className="text-sm2"
              />
            </label>
            <p className="text-caption text-muted-foreground">{consequence(req)}</p>
            {error && <p className="text-caption text-destructive">{error}</p>}
            <div className="flex gap-2">
              <Button size="sm" disabled={!canApprove} onClick={() => decide("approve")}>
                Approve
              </Button>
              <Button size="sm" variant="destructive" disabled={busy || note.trim() === ""} onClick={() => decide("deny")}>
                Deny
              </Button>
            </div>
          </div>
        ) : (
          <div className="border-t border-border pt-4 text-caption text-muted-foreground">
            {req.state} by {req.decidedBy ?? "someone"} {when(new Date(req.decidedAt ?? 0).getTime())}
            {req.note ? ` · ${req.note}` : ""}
          </div>
        )}
      </div>
    </Section>
  );
}
