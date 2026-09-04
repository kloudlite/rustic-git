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
  verbs,
  decommissionStatus,
}: {
  region: string;
  node: string;
  /** From `nodeVerbs` — the caller decides what this node may be asked to do. */
  verbs: ("drain" | "undrain" | "decommission" | "delete-vm")[];
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

  return (
    <>
      {error && <span className="text-caption text-destructive">{error}</span>}
      {verbs.map((v) => (
        <button
          key={v}
          type="button"
          className="text-sm2 text-muted-foreground hover:text-primary disabled:cursor-not-allowed disabled:opacity-40"
          // `decommission` is the cordon and nothing more — the `drained` stamp is its gate, and
          // deleting the VM stays a human step outside this console (CLAUDE.md). So the verb the
          // mockup calls "Delete VM" is this same button, finally enabled.
          disabled={v === "decommission" && !isDrained(decommissionStatus)}
          onClick={() => open(v === "delete-vm" ? "decommission" : v)}
        >
          {v === "delete-vm" ? "Decommission" : v[0].toUpperCase() + v.slice(1)}
        </button>
      ))}
      {verb && dialog()}
    </>
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
