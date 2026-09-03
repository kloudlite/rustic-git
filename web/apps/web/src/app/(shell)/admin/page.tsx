import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { when } from "@/lib/time";
import { dimLabel, type QuotaDim } from "@/lib/quota";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { decideRequest } from "./actions";

export const metadata: Metadata = { title: "Quota queue" };

function ageOf(createdAt: string | null | undefined) {
  return createdAt ? when(new Date(createdAt).getTime()) : "unknown";
}

function dims(requested: Partial<Record<QuotaDim, number>>) {
  return Object.entries(requested)
    .map(([d, v]) => `${dimLabel(d as QuotaDim)} → ${v}`)
    .join(", ");
}

export default async function Page() {
  const { token } = await requireSuperadmin("/admin");
  const r = await api.adminListQuotaRequests(token);
  const rows = r.ok ? r.value : [];
  const pending = rows.filter((row) => row.state === "pending");
  // Newest-first already, from the api; a handful is context for a decision just made, not a log.
  const decided = rows.filter((row) => row.state !== "pending").slice(0, 10);

  return (
    <div className="space-y-8">
      <section>
        <h2 className="mb-3 text-sm2 font-medium">Pending ({pending.length})</h2>
        {pending.length === 0 ? (
          <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
            Nothing waiting.
          </p>
        ) : (
          <ul className="divide-y divide-border border border-border bg-card">
            {pending.map((req) => (
              <li key={req.id} className="space-y-3 px-4 py-4">
                <div className="flex items-center justify-between gap-3">
                  <span className="text-sm2 font-medium">{req.owner}</span>
                  <span className="text-caption text-muted-foreground">{ageOf(req.createdAt)}</span>
                </div>
                <p className="text-sm2 text-muted-foreground">{dims(req.requested)}</p>
                {req.reason && <p className="text-sm2">{req.reason}</p>}
                <form className="flex items-center gap-2">
                  <Input name="note" placeholder="Note (optional)" className="h-8 flex-1" />
                  <Button formAction={decideRequest.bind(null, req.id, "approve")} size="sm">
                    Approve
                  </Button>
                  <Button
                    formAction={decideRequest.bind(null, req.id, "deny")}
                    variant="outline"
                    size="sm"
                  >
                    Deny
                  </Button>
                </form>
              </li>
            ))}
          </ul>
        )}
      </section>

      {decided.length > 0 && (
        <section>
          <h2 className="mb-3 text-sm2 font-medium">Recently decided</h2>
          <ul className="divide-y divide-border border border-border bg-card">
            {decided.map((req) => (
              <li key={req.id} className="flex items-center justify-between gap-3 px-4 py-3 text-sm2">
                <span className="font-medium">{req.owner}</span>
                <span className="text-muted-foreground">{dims(req.requested)}</span>
                <span className={req.state === "approved" ? "text-primary" : "text-destructive"}>
                  {req.state}
                </span>
                <span className="text-caption text-muted-foreground">{ageOf(req.decidedAt)}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
