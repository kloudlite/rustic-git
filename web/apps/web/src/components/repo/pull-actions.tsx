"use client";

import { useActionState } from "react";
import { GitMerge, Loader2, TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { close, comment, merge, type PullState as ActionState } from "@/app/[owner]/[repo]/pulls/actions";
import type { PullState } from "@/lib/api";

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
  canFastForward,
  unrelated,
}: {
  owner: string;
  repo: string;
  number: number;
  state: PullState;
  baseBranch: string;
  canFastForward: boolean;
  unrelated: boolean;
}) {
  const [result, mergeAction, merging] = useActionState<ActionState, FormData>(merge, null);
  if (state !== "open") return null;

  return (
    <div className="mt-6 border border-border bg-card p-4">
      {canFastForward ? (
        <form action={mergeAction} className="flex flex-wrap items-center gap-3">
          <Which owner={owner} repo={repo} number={number} />
          <Button type="submit" disabled={merging}>
            {merging ? <Loader2 className="animate-spin" /> : <GitMerge />}
            Merge into <span className="font-mono">{baseBranch}</span>
          </Button>
          <p className="text-caption text-muted-foreground">
            Fast-forward: <span className="font-mono">{baseBranch}</span> moves to this branch, so nothing is rewritten.
          </p>
        </form>
      ) : (
        <p className="flex items-start gap-2 text-sm2 leading-relaxed text-muted-foreground">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" />
          {unrelated
            ? "These branches share no history, so there is nothing to merge."
            : "The base has moved on since this branch left it. Rebase onto the base and push again — this server merges by fast-forward only, so it will not rewrite anyone's history to do it."}
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
