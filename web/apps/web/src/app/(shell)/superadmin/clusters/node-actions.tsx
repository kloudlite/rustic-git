"use client";

import { useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { isDrained } from "@/lib/clusters";
import { drainNodeAction, undrainNodeAction, decommissionNodeAction } from "../actions";

type Verb = "drain" | "undrain" | "decommission";

const VERB_COPY: Record<Verb, { label: string; title: string; second: string }> = {
  drain: {
    label: "Drain",
    title: "This node keeps running what's on it",
    second: "Running work keeps running here, but nothing new lands on it — it starts elsewhere from now on.",
  },
  undrain: { label: "Undrain", title: "", second: "" },
  decommission: {
    label: "Decommission",
    title: "The node is cordoned",
    second: "The node is cordoned; its VM may now be deleted. The console never deletes it — that's an operator step outside this page.",
  },
};

/** Drain, Undrain, Decommission — a required reason on every one (Global Constraint), a second
 *  confirmation naming the consequence for drain and decommission, none for undrain (the
 *  documented abort — CLAUDE.md, "Workspaces and environments"). */
export function NodeActions({
  region,
  node,
  decommission,
  decommissionStatus,
}: {
  region: string;
  node: string;
  decommission: boolean;
  decommissionStatus: string | null;
}) {
  const router = useRouter();
  const [verb, setVerb] = useState<Verb | null>(null);
  const [reason, setReason] = useState("");
  const [confirmedFirst, setConfirmedFirst] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  function open(v: Verb) {
    setVerb(v);
    setReason("");
    setConfirmedFirst(false);
    setError(null);
  }

  function submit() {
    if (!verb || reason.trim() === "") return;
    const needsSecond = verb !== "undrain";
    if (needsSecond && !confirmedFirst) {
      setConfirmedFirst(true);
      return;
    }
    startTransition(async () => {
      const action = verb === "drain" ? drainNodeAction : verb === "undrain" ? undrainNodeAction : decommissionNodeAction;
      const r = await action(region, node, reason.trim());
      if (!r.ok) {
        setError(r.message);
        return;
      }
      setVerb(null);
      router.refresh();
    });
  }

  if (decommission) {
    return (
      <div className="flex items-center justify-end gap-2">
        {error && <span className="text-caption text-destructive">{error}</span>}
        <Button type="button" size="sm" variant="outline" onClick={() => open("undrain")}>Undrain</Button>
        <Button type="button" size="sm" variant="outline" disabled={!isDrained(decommissionStatus)} onClick={() => open("decommission")}>
          Decommission
        </Button>
        {verb && dialog()}
      </div>
    );
  }

  return (
    <div className="flex items-center justify-end gap-2">
      {error && <span className="text-caption text-destructive">{error}</span>}
      <Button type="button" size="sm" variant="outline" onClick={() => open("drain")}>Drain</Button>
      {verb && dialog()}
    </div>
  );

  function dialog() {
    const copy = VERB_COPY[verb!];
    return (
      <Dialog open onOpenChange={(o) => !o && setVerb(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{confirmedFirst ? copy.title : `${copy.label} ${node}`}</DialogTitle>
            <DialogDescription>
              {confirmedFirst ? copy.second : "A reason is required — it's recorded on the node alongside who and when."}
            </DialogDescription>
          </DialogHeader>
          {!confirmedFirst && (
            <Textarea value={reason} onChange={(e) => setReason(e.target.value)} placeholder={`Why is this node being ${verb === "undrain" ? "undrained" : `${verb}ed`}?`} rows={3} />
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setVerb(null)}>Cancel</Button>
            <Button type="button" onClick={submit} disabled={pending || reason.trim() === ""}>
              {pending && <Loader2 className="animate-spin" />}
              {confirmedFirst ? "Continue" : copy.label}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  }
}
