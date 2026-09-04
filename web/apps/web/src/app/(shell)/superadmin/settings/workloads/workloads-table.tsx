"use client";

import { useState, useTransition } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { rolloutStateLabel, settled } from "@/lib/settings";
import type { WorkloadDoc, AdminNode } from "@/lib/api";
import type { SaveResult } from "../actions";

/** Read-only per spec §6, except the one manual roll — same reason-required, second-confirm-for-
 *  `rustic-git-srv` pattern `SettingsTable`'s save-and-roll dialog uses, since a manual roll of the
 *  StatefulSet carries the same DB-ownership-move risk either way it's triggered. */
export function WorkloadsTable({
  workloads,
  nodes,
  onRoll,
}: {
  workloads: WorkloadDoc[];
  nodes: AdminNode[];
  onRoll: (scope: string, name: string, reason: string) => Promise<SaveResult>;
}) {
  const [target, setTarget] = useState<{ scope: string; name: string } | null>(null);
  const [reason, setReason] = useState("");
  const [confirmedFirst, setConfirmedFirst] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  const anyRollingOut = workloads.some((w) => !settled(w));

  function openDialog(scope: string, name: string) {
    setTarget({ scope, name });
    setReason("");
    setConfirmedFirst(false);
    setError(null);
  }

  function submit() {
    if (!target || reason.trim() === "") return;
    const needsSecond = target.name === "rustic-git-srv";
    if (needsSecond && !confirmedFirst) {
      setConfirmedFirst(true);
      return;
    }
    startTransition(async () => {
      const result = await onRoll(target.scope, target.name, reason.trim());
      if (!result.ok) {
        setError(result.message);
        return;
      }
      setTarget(null);
    });
  }

  return (
    <div className="space-y-6">
      {anyRollingOut && <AutoRefresh intervalMs={3_000} />}

      <div className="overflow-x-auto border border-border bg-card">
        <table className="w-full text-sm2">
          <thead className="border-b border-border text-left text-caption text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">Workload</th>
              <th className="px-3 py-2 font-medium">Scope</th>
              <th className="px-3 py-2 font-medium">Image</th>
              <th className="px-3 py-2 font-medium">Rollout</th>
              <th className="px-3 py-2 font-medium">Last roll</th>
              <th className="px-3 py-2 font-medium" />
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {workloads.map((w) => (
              <tr key={`${w.scope}/${w.name}`}>
                <td className="px-3 py-2 align-top font-medium">{w.name}</td>
                <td className="px-3 py-2 align-top text-caption text-muted-foreground">{w.scope}</td>
                <td className="px-3 py-2 align-top text-caption text-muted-foreground">
                  {digestOrTag(w.image)}
                </td>
                <td className="px-3 py-2 align-top text-caption">
                  {rolloutStateLabel(w.rolloutState, w.ready, w.desired)}
                </td>
                <td className="px-3 py-2 align-top text-caption text-muted-foreground">
                  {w.lastRoll ? `${w.lastRoll.by} · ${w.lastRoll.at} · ${w.lastRoll.reason}` : "—"}
                </td>
                <td className="px-3 py-2 align-top">
                  <Button size="sm" variant="outline" onClick={() => openDialog(w.scope, w.name)}>
                    Roll
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <div>
        <h2 className="mb-2 text-sm2 font-medium">Nodes</h2>
        <ul className="divide-y divide-border border border-border bg-card">
          {nodes.length === 0 ? (
            <li className="px-4 py-8 text-center text-sm2 text-muted-foreground">No nodes reported.</li>
          ) : (
            nodes.map((n) => (
              <li key={n.name} className="flex items-center justify-between gap-3 px-4 py-3 text-sm2">
                <span className="font-medium">{n.name}</span>
                <span className={n.ready ? "text-muted-foreground" : "text-destructive"}>
                  {n.ready ? "Ready" : "Not ready"}
                </span>
                <span className="text-caption text-muted-foreground">
                  {n.decommission ? (n.decommissionStatus ?? "decommissioning") : ""}
                </span>
              </li>
            ))
          )}
        </ul>
      </div>

      <Dialog open={target !== null} onOpenChange={(open) => !open && setTarget(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{confirmedFirst ? "This rolls rustic-git-srv" : `Roll ${target?.name}`}</DialogTitle>
            <DialogDescription>
              {confirmedFirst
                ? "Rolling rustic-git-srv moves database ownership between nodes (CLAUDE.md, “Deploying”) — a brief window where the first registry request to a moved image can fail once."
                : "A reason is required — it's recorded on the workload alongside who and when."}
            </DialogDescription>
          </DialogHeader>
          {!confirmedFirst && (
            <Textarea
              value={reason}
              onChange={(e) => setReason(e.target.value)}
              placeholder="Why is this roll needed?"
              rows={3}
            />
          )}
          {error && <p role="alert" className="text-sm2 font-medium text-destructive">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setTarget(null)}>Cancel</Button>
            <Button type="button" onClick={submit} disabled={pending || reason.trim() === ""}>
              {pending && <Loader2 className="animate-spin" />}
              {confirmedFirst ? "Continue" : "Roll"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

/** Images are `repo:tag` or `repo@sha256:hex` — either way the whole string is what the operator
 *  wants to compare against a known-good pin, so this only handles the missing case. */
function digestOrTag(image: string | null): string {
  return image ?? "—";
}
