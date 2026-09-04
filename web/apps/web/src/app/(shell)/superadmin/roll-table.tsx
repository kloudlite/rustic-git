"use client";

import { useState, useTransition } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { when } from "@/lib/time";
import { settled } from "@/lib/settings";
import type { WorkloadDoc } from "@/lib/api";
import type { SaveResult } from "./actions";
import { RolloutBadge } from "./status-badge";

/** Image tag + digest, ready/desired, rollout state, last roll who/when/reason, and the one
 *  manual write — a required reason, with a second confirmation for `rustic-git-srv` since
 *  rolling the StatefulSet moves database ownership between nodes (CLAUDE.md, "Deploying"). Used
 *  by both Monitoring (central workloads) and each Clusters region panel (that region's agent
 *  DaemonSet and gateway). */
export function RollTable({
  workloads,
  onRoll,
}: {
  workloads: WorkloadDoc[];
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

  if (workloads.length === 0) {
    return <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">No workloads reported.</p>;
  }

  return (
    <div className="space-y-4">
      {anyRollingOut && <AutoRefresh intervalMs={3_000} />}

      <div className="overflow-x-auto border border-border bg-card">
        <table className="w-full text-sm2">
          <thead className="border-b border-border text-left text-caption text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">Workload</th>
              <th className="px-3 py-2 font-medium">Tag</th>
              <th className="px-3 py-2 font-medium">Digest</th>
              <th className="px-3 py-2 font-medium">Ready</th>
              <th className="px-3 py-2 font-medium">Rollout</th>
              <th className="px-3 py-2 font-medium">Last roll</th>
              <th className="px-3 py-2 font-medium" />
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {workloads.map((w) => {
              const { tag, digest } = imageRef(w.image);
              return (
                <tr key={`${w.scope}/${w.name}`}>
                  <td className="px-3 py-2 align-top font-medium">{w.name}</td>
                  <td className="px-3 py-2 align-top text-caption text-muted-foreground">{tag}</td>
                  <td className="px-3 py-2 align-top text-caption text-muted-foreground font-mono">{digest}</td>
                  <td className="px-3 py-2 align-top text-caption tabular-nums">{w.ready}/{w.desired}</td>
                  <td className="px-3 py-2 align-top text-caption"><RolloutBadge w={w} /></td>
                  <td className="px-3 py-2 align-top text-caption text-muted-foreground">
                    {w.lastRoll ? `${w.lastRoll.by} · ${when(new Date(w.lastRoll.at).getTime())} · ${w.lastRoll.reason}` : "—"}
                  </td>
                  <td className="px-3 py-2 align-top">
                    <Button size="sm" variant="outline" onClick={() => openDialog(w.scope, w.name)}>
                      Roll
                    </Button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
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

/** `deploy/pin.sh` pins `repo:tag@sha256:hex` — split on `@` so the table can show the
 *  human-readable tag and the verifiable digest as two columns instead of one long string. */
function imageRef(image: string | null): { tag: string; digest: string } {
  if (!image) return { tag: "—", digest: "—" };
  const [tag, digest] = image.split("@");
  return { tag, digest: digest ?? "—" };
}
