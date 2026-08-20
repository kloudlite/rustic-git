"use client";

import { useActionState, useState } from "react";
import { Check, ChevronDown, CircleCheck, CircleDashed, CircleX, GitMerge, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import { close, comment, merge, type PullState as ActionState } from "@/app/(shell)/[owner]/[repo]/pulls/actions";
import type { ApiMergeJob, ApiMergeability, PullState } from "@/lib/api";

function Which({ owner, repo, number }: { owner: string; repo: string; number: number }) {
  return (
    <>
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="repo" value={repo} />
      <input type="hidden" name="number" value={number} />
    </>
  );
}

/**
 * Merge and close.
 *
 * Merging is fast-forward only, so the button is offered only when the base can
 * actually move — and when it cannot, the reason is stated rather than the button
 * being left there to fail. The server checks again anyway: the branch can move
 * between this page rendering and the click.
 */
export function PullActions({
  owner,
  repo,
  number,
  state,
  baseBranch,
  mergeability,
  job,
}: {
  owner: string;
  repo: string;
  number: number;
  state: PullState;
  baseBranch: string;
  /** From the worker, not from this render. Absent means it has not looked yet. */
  mergeability?: ApiMergeability;
  job?: ApiMergeJob;
}) {
  const [result, mergeAction, merging] = useActionState<ActionState, FormData>(merge, null);
  // Fast-forward first because it is the only one that creates no commit: the
  // base simply moves, so nothing is invented and nothing is rewritten.
  const [strategy, setStrategy] = useState("fast-forward");
  const [open, setOpen] = useState(false);
  if (state !== "open") return null;

  // A merge already asked for. Shown instead of the button, because the answer to
  // "can I merge this" is now "it is being merged".
  if (job && (job.state === "queued" || job.state === "running")) {
    return (
      <p className="mt-6 flex items-center gap-2 border border-border bg-card px-4 py-3 text-sm2">
        <Loader2 className="size-4 shrink-0 animate-spin text-muted-foreground" />
        {job.state === "queued" ? "Waiting to merge…" : "Merging…"}
        <span className="text-muted-foreground">({job.strategy})</span>
      </p>
    );
  }

  const canFastForward = mergeability?.state === "clean";
  const unknownYet = !mergeability || mergeability.state === "unknown";

  const strategies = [
    {
      value: "fast-forward",
      label: "Fast-forward",
      detail: `${baseBranch} moves to this branch. No new commit, nothing rewritten.`,
    },
    {
      value: "squash",
      label: "Squash and merge",
      detail: "One new commit on the base with all of this branch's changes. The branch's own commits are not kept.",
    },
    {
      value: "merge",
      label: "Merge commit",
      detail: "A new commit with two parents, keeping this branch's history alongside the base's.",
    },
    {
      value: "rebase",
      label: "Rebase and merge",
      detail: "This branch's commits are replayed onto the base, without a merge commit.",
    },
  ];

  const label =
    strategy === "squash" ? "Squash and merge"
    : strategy === "merge" ? "Create a merge commit"
    : strategy === "rebase" ? "Rebase and merge"
    : "Merge pull request";

  return (
    <div className="mt-6 border border-border bg-card">
      {/* The things that gate a merge, each with its own verdict -- so "why can
          I not merge this" is answered on the row that says no, not inferred
          from a missing button. */}
      <ul className="divide-y divide-border">
        <li className="flex items-start gap-3 px-4 py-3">
          {unknownYet ? (
            <Loader2 className="mt-0.5 size-4 shrink-0 animate-spin text-muted-foreground" />
          ) : canFastForward ? (
            <CircleCheck className="mt-0.5 size-4 shrink-0 text-success" />
          ) : (
            <CircleX className="mt-0.5 size-4 shrink-0 text-destructive" />
          )}
          <div className="min-w-0 flex-1">
            <div className="text-sm2 font-medium">
              {unknownYet
                ? "Checking whether this can be merged"
                : canFastForward
                  ? "No conflicts with the base branch"
                  : "This cannot be merged as it stands"}
            </div>
            <div className="text-caption text-muted-foreground">
              {unknownYet
                ? "The worker has not looked at this yet."
                : canFastForward
                  ? "Merging can be performed automatically."
                  : mergeability?.detail ?? `${baseBranch} has moved on since this branch left it.`}
            </div>
          </div>
        </li>
        <li className="flex items-start gap-3 px-4 py-3">
          <CircleDashed className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
          <div className="min-w-0 flex-1">
            <div className="text-sm2 font-medium">No checks configured</div>
            <div className="text-caption text-muted-foreground">Nothing is gating this change.</div>
          </div>
        </li>
      </ul>

      {canFastForward ? (
        <form action={mergeAction} className="flex flex-wrap items-center gap-3 border-t border-border bg-muted/40 px-4 py-3">
          <Which owner={owner} repo={repo} number={number} />
          <input type="hidden" name="strategy" value={strategy} />
          <div className="flex items-stretch">
            <Button type="submit" disabled={merging} className="rounded-none">
              {merging ? <Loader2 className="animate-spin" /> : <GitMerge />}
              {label}
            </Button>
            <button
              type="button"
              aria-label="Choose how to merge"
              aria-expanded={open}
              onClick={() => setOpen((o) => !o)}
              className="flex items-center border-l border-primary-foreground/25 bg-primary px-2 text-primary-foreground transition-colors hover:bg-primary/90"
            >
              <ChevronDown className="size-4" />
            </button>
          </div>
          <span className="text-caption text-muted-foreground">
            into <span className="font-mono text-foreground/80">{baseBranch}</span>
          </span>

          {open && (
            <ul className="w-full border border-border bg-card">
              {strategies.map((st) => (
                <li key={st.value}>
                  <button
                    type="button"
                    onClick={() => { setStrategy(st.value); setOpen(false); }}
                    className={cn(
                      "flex w-full items-start gap-3 px-3 py-2.5 text-left transition-colors hover:bg-muted/60",
                      strategy === st.value && "bg-muted/40",
                    )}
                  >
                    <Check className={cn("mt-0.5 size-4 shrink-0", strategy === st.value ? "text-primary" : "text-transparent")} />
                    <span>
                      <span className="block text-sm2 font-medium">{st.label}</span>
                      <span className="block text-caption text-muted-foreground">{st.detail}</span>
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </form>
      ) : null}

      {job?.state === "conflicts" && (
        <p role="alert" className="mt-3 border-l-2 border-warning pl-3 text-sm2 text-muted-foreground">
          The last attempt stopped: {job.detail ?? "the branches conflict."}
        </p>
      )}
      {job?.state === "failed" && (
        <p role="alert" className="mt-3 text-sm2 font-medium text-destructive">
          The last attempt failed: {job.detail ?? "unknown error"}. You can try again.
        </p>
      )}
      {result?.error && (
        <p role="alert" className="mt-3 text-sm2 font-medium text-destructive">{result.error}</p>
      )}

      <form action={close} className="mt-3 border-t border-border pt-3">
        <Which owner={owner} repo={repo} number={number} />
        <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive">
          Close without merging
        </Button>
      </form>
    </div>
  );
}

export function CommentBox({ owner, repo, number }: { owner: string; repo: string; number: number }) {
  const [state, action, pending] = useActionState<ActionState, FormData>(comment, null);
  return (
    <form action={action} className="mt-6 grid gap-3">
      <Which owner={owner} repo={repo} number={number} />
      <Textarea name="body" rows={3} placeholder="Leave a comment" className="resize-y text-sm2" aria-label="Comment" />
      {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
      <div>
        <Button type="submit" variant="outline" className="border-edge hover:border-edge-hover" disabled={pending}>
          {pending && <Loader2 className="animate-spin" />}Comment
        </Button>
      </div>
    </form>
  );
}
