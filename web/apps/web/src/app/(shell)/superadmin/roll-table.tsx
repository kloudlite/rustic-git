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
import { Section } from "./ui/section";
import { DataTable, EmptyState, RowActions, Td, Th, Tr } from "./ui/data-table";

/** Image tag + digest, ready/desired, rollout state, last roll who/when/reason, and the one
 *  manual write — a required reason, with a second confirmation for `kloudlite-git-srv` since
 *  rolling the StatefulSet moves database ownership between nodes (CLAUDE.md, "Deploying"). Used
 *  by both Monitoring (central workloads) and each Clusters region panel (that region's agent
 *  DaemonSet and gateway). */
export function RollTable({
  workloads,
  onRoll,
  restarts,
  title = "Central workloads",
}: {
  workloads: WorkloadDoc[];
  onRoll: (scope: string, name: string, reason: string) => Promise<SaveResult>;
  /** Restart count per workload name, from the signals response — optional because the Clusters
   *  tab renders the same table without a scrape behind it. */
  restarts?: Record<string, number>;
  title?: string;
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
    const needsSecond = target.name === "kloudlite-git-srv";
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
    return (
      <Section eyebrow="Workloads" title={title} count={0} bare>
        <EmptyState>No workload is reported for this scope. One appears here as soon as its Deployment exists.</EmptyState>
      </Section>
    );
  }

  return (
    <div className="space-y-4">
      {anyRollingOut && <AutoRefresh intervalMs={3_000} />}

      <Section eyebrow="Workloads" title={title} count={workloads.length} bare>
        <DataTable>
          <thead>
            <tr>
              <Th>Workload</Th>
              <Th>Image tag</Th>
              <Th>Digest</Th>
              <Th numeric>Ready</Th>
              <Th>Rollout</Th>
              <Th numeric>Restarts</Th>
              <Th>Last roll</Th>
              <Th />
            </tr>
          </thead>
          <tbody>
            {workloads.map((w) => {
              const { tag, digest } = imageRef(w.image);
              return (
                <Tr key={`${w.scope}/${w.name}`}>
                  <Td className="font-medium">{w.name}</Td>
                  <Td className="text-muted-foreground">{tag}</Td>
                  <Td className="font-mono text-caption text-muted-foreground">{digest}</Td>
                  <Td numeric>{w.ready} / {w.desired}</Td>
                  <Td><RolloutBadge w={w} /></Td>
                  <Td numeric className={restarts?.[w.name] ? "text-warning" : "text-muted-foreground"}>
                    {restarts?.[w.name] ?? 0}
                  </Td>
                  <Td className="text-muted-foreground">
                    {w.lastRoll ? `${w.lastRoll.by} · ${when(new Date(w.lastRoll.at).getTime())} · ${w.lastRoll.reason}` : "—"}
                  </Td>
                  <Td>
                    <RowActions>
                      <button
                        type="button"
                        className="text-sm2 text-muted-foreground hover:text-foreground"
                        onClick={() => openDialog(w.scope, w.name)}
                      >
                        Roll
                      </button>
                    </RowActions>
                  </Td>
                </Tr>
              );
            })}
          </tbody>
        </DataTable>
      </Section>

      <Dialog open={target !== null} onOpenChange={(open) => !open && setTarget(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{confirmedFirst ? "This rolls kloudlite-git-srv" : `Roll ${target?.name}`}</DialogTitle>
            <DialogDescription>
              {confirmedFirst
                ? "Rolling kloudlite-git-srv moves database ownership between nodes (CLAUDE.md, “Deploying”) — a brief window where the first registry request to a moved image can fail once."
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
