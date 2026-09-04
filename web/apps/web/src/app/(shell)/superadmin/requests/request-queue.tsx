"use client";

import { useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { OwnerUsage, QuotaRequestDoc } from "@/lib/api";
import { DIMS, dimLabel, requestedDiffs, type QuotaDim } from "@/lib/quota";
import { when } from "@/lib/time";
import { DecisionPanel } from "./decision-panel";

/** The whole queue is small enough that one fetch covers Pending, Decided, and a row's own
 *  history — client-side filtering over it (owner substring, dimension, age) is the honest lazy
 *  answer over adding a server-side shape for narrowing this small a list. */
export function RequestQueue({ rows, usage }: { rows: QuotaRequestDoc[]; usage: OwnerUsage[] }) {
  const router = useRouter();
  const usageByOwner = useMemo(() => new Map(usage.map((u) => [u.owner, u])), [usage]);

  const [tab, setTab] = useState<"pending" | "decided">("pending");
  const [q, setQ] = useState("");
  const [dim, setDim] = useState<QuotaDim | "any">("any");
  const [oldestFirst, setOldestFirst] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);

  const pending = rows.filter((r) => r.state === "pending");
  const decided = rows.filter((r) => r.state !== "pending");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const base = tab === "pending" ? pending : decided;
    const filtered = base.filter((r) => {
      if (needle && !r.owner.toLowerCase().includes(needle)) return false;
      if (dim !== "any" && r.requested[dim] === undefined) return false;
      return true;
    });
    const byAge = [...filtered].sort((a, b) => new Date(a.createdAt ?? 0).getTime() - new Date(b.createdAt ?? 0).getTime());
    return oldestFirst ? byAge : byAge.reverse();
  }, [tab, pending, decided, q, dim, oldestFirst]);

  const active = rows.find((r) => r.id === selected) ?? null;

  function decided_() {
    setSelected(null);
    router.refresh();
  }

  return (
    <div className="flex gap-5">
      <div className="flex min-w-0 flex-1 flex-col gap-3">
        <div className="flex items-center gap-2">
          <Tabs value={tab} onValueChange={(v) => { setTab(v as typeof tab); setSelected(null); }}>
            <TabsList className="h-8">
              <TabsTrigger value="pending" className="text-sm2">
                Pending<span className="ml-1.5 text-muted-foreground">{pending.length}</span>
              </TabsTrigger>
              <TabsTrigger value="decided" className="text-sm2">
                Decided<span className="ml-1.5 text-muted-foreground">{decided.length}</span>
              </TabsTrigger>
            </TabsList>
          </Tabs>
          <div className="ml-auto flex items-center gap-2">
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Filter owner"
              aria-label="Filter by owner"
              className="h-8 w-40 text-sm2"
            />
            <Select value={dim} onValueChange={(v) => setDim(v as typeof dim)}>
              <SelectTrigger className="h-8 text-sm2"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="any">any dimension</SelectItem>
                {DIMS.map((d) => (
                  <SelectItem key={d} value={d}>{dimLabel(d)}</SelectItem>
                ))}
              </SelectContent>
            </Select>
            <button
              type="button"
              onClick={() => setOldestFirst((v) => !v)}
              className="h-8 border border-border px-2.5 text-sm2 text-muted-foreground hover:bg-muted"
            >
              {oldestFirst ? "oldest first" : "newest first"}
            </button>
          </div>
        </div>

        {shown.length === 0 ? (
          <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
            Nothing matches that.
          </p>
        ) : (
          <ul className="divide-y divide-border border border-border bg-card">
            {shown.map((r) => {
              const limit = usageByOwner.get(r.owner)?.limit;
              const diffs = limit ? requestedDiffs(limit, r.requested) : [];
              return (
                <li key={r.id}>
                  <button
                    type="button"
                    onClick={() => setSelected(r.id)}
                    className={`flex w-full items-center gap-3 px-4 py-3 text-left text-sm2 hover:bg-muted/50 ${selected === r.id ? "bg-muted" : ""}`}
                  >
                    <span className="w-32 shrink-0 truncate font-medium">{r.owner}</span>
                    <span className="min-w-0 flex-1 truncate text-muted-foreground">
                      {diffs.length > 0
                        ? diffs.map((d) => `${dimLabel(d.dim)} ${d.from} → ${d.to}`).join(", ")
                        : DIMS.filter((d) => r.requested[d] !== undefined).map((d) => dimLabel(d)).join(", ")}
                    </span>
                    {tab === "decided" && (
                      <Badge variant={r.state === "approved" ? "outline" : "destructive"} className="capitalize">
                        {r.state}
                      </Badge>
                    )}
                    <span className="shrink-0 text-caption text-muted-foreground">
                      {when(new Date((tab === "decided" ? r.decidedAt : r.createdAt) ?? 0).getTime())}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </div>

      {active && active.state === "pending" && (
        <DecisionPanel request={active} usage={usageByOwner.get(active.owner)} all={rows} onDecided={decided_} />
      )}
    </div>
  );
}
