"use client";

import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import type { QuotaRequestDoc } from "@/lib/api";
import { DIMS, dimLabel, requestedDiffs, type QuotaDim } from "@/lib/quota";
import { when } from "@/lib/time";
import { decideRequest } from "../actions";

/** The row a request is decided against: current limit and in-use count per dimension, so the
 *  panel needs no fetch of its own — the page already has `adminUsage` for every owner shown. */
type OwnerUsageRow = { limit: Record<QuotaDim, number>; used: Record<QuotaDim, number> };

const ZERO: Record<QuotaDim, number> = { workspaces: 0, environments: 0, snapshots: 0, diskGb: 0, cpu: 0, memoryGb: 0 };

/** The facts behind one pending request, an editable grant, and the owner's recent history —
 *  everything a decision needs without a second click. `all` is the whole fetched queue (page
 *  already has it), filtered here to this owner's last three DECIDED requests rather than a
 *  second round trip. */
export function DecisionPanel({
  request,
  usage,
  all,
  onDecided,
}: {
  request: QuotaRequestDoc;
  usage: OwnerUsageRow | undefined;
  all: QuotaRequestDoc[];
  onDecided: () => void;
}) {
  const limit = usage?.limit ?? ZERO;
  const used = usage?.used ?? ZERO;
  const diffs = requestedDiffs(limit, request.requested);

  const [grant, setGrant] = useState<Partial<Record<QuotaDim, number>>>(request.requested);
  const [note, setNote] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const history = all
    .filter((r) => r.owner === request.owner && r.state !== "pending" && r.id !== request.id)
    .sort((a, b) => new Date(b.decidedAt ?? 0).getTime() - new Date(a.decidedAt ?? 0).getTime())
    .slice(0, 3);

  async function submit(decision: "approve" | "deny") {
    if (decision === "deny" && !note.trim()) {
      setError("A note is required to deny.");
      return;
    }
    setPending(true);
    setError(null);
    const r = await decideRequest(request.id, decision, note.trim(), decision === "approve" ? grant : undefined);
    setPending(false);
    if (!r.ok) {
      setError(r.message);
      return;
    }
    onDecided();
  }

  return (
    <div className="flex w-[26rem] shrink-0 flex-col gap-4 border border-border bg-card p-4">
      <div className="flex items-center justify-between gap-3">
        <span className="text-sm2 font-medium">{request.owner}</span>
        <span className="text-caption text-muted-foreground">asked {when(new Date(request.createdAt ?? 0).getTime())}</span>
      </div>
      {request.reason && <p className="text-sm2 text-muted-foreground">&ldquo;{request.reason}&rdquo;</p>}

      <div className="flex flex-col gap-3">
        {diffs.map(({ dim, from, to }) => (
          <div key={dim} className="flex flex-col gap-1">
            <div className="flex items-center justify-between text-caption text-muted-foreground">
              <span>{dimLabel(dim)}</span>
              <span>
                {used[dim]} of {from} in use
              </span>
            </div>
            <div className="h-2 bg-muted" role="presentation">
              <div className="h-2 bg-primary" style={{ width: `${Math.min(100, Math.round((used[dim] / Math.max(from, 1)) * 100))}%` }} />
            </div>
            <div className="flex items-center gap-2">
              <span className="text-sm2 tabular-nums text-muted-foreground">{from} →</span>
              <Input
                type="number"
                min={0}
                value={grant[dim] ?? to}
                onChange={(e) => setGrant((g) => ({ ...g, [dim]: Number(e.target.value) }))}
                className="h-8 w-24"
                aria-label={`Grant for ${dimLabel(dim)}`}
              />
            </div>
          </div>
        ))}
      </div>

      {history.length > 0 && (
        <div className="flex flex-col gap-1.5 border-t border-border pt-3">
          <p className="text-caption text-muted-foreground">Last decisions for {request.owner}</p>
          {history.map((h) => (
            <div key={h.id} className="flex items-center gap-2 text-caption">
              <Badge variant={h.state === "approved" ? "outline" : "destructive"} className="capitalize">
                {h.state}
              </Badge>
              <span className="truncate text-muted-foreground">
                {DIMS.filter((d) => h.requested[d] !== undefined).map((d) => dimLabel(d)).join(", ")}
                {h.decidedBy ? ` · ${h.decidedBy}` : ""}
                {h.decidedAt ? ` · ${when(new Date(h.decidedAt).getTime())}` : ""}
              </span>
            </div>
          ))}
        </div>
      )}

      <div className="mt-auto flex flex-col gap-2">
        <Textarea
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="Note (required to deny, optional to approve)"
          className="h-16 resize-none text-sm2"
        />
        {error && <p className="text-sm2 text-destructive">{error}</p>}
        <div className="flex justify-end gap-2">
          <Button variant="destructive" size="sm" disabled={pending} onClick={() => submit("deny")}>
            Deny
          </Button>
          <Button size="sm" disabled={pending} onClick={() => submit("approve")}>
            Approve
          </Button>
        </div>
      </div>
    </div>
  );
}
