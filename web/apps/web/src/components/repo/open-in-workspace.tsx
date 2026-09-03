"use client";

import { useActionState } from "react";
import { Loader2, SquareTerminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import { openInWorkspace, type WsActionState } from "@/app/(shell)/[owner]/(org)/workspaces/actions";
import { QuotaRequestDialog } from "@/components/app/quota-request-dialog";

/** One button, two homes: the repo Clone menu and the PR header. Both want the same
 *  thing — a workspace with this repo on this branch — so the form lives here once and
 *  the callers only differ in how the button is sized. */
export function OpenInWorkspace({
  owner, repo, branch, label = "Open in a workspace", className, size,
}: {
  owner: string;
  repo: string;
  branch: string;
  label?: string;
  className?: string;
  size?: "sm" | "default";
}) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(openInWorkspace, null);
  return (
    <form action={action}>
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="repo" value={repo} />
      <input type="hidden" name="branch" value={branch} />
      <Button type="submit" variant="outline" size={size} disabled={pending} className={className}>
        {pending ? <Loader2 className="animate-spin" /> : <SquareTerminal />}{label}
      </Button>
      {state?.error && (
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <p role="alert" className="text-caption font-medium text-destructive">{state.error}</p>
          {/* Only when the refusal named a quota dimension — a name-collision 409 gets no
              request trigger, since raising quota would not fix it. */}
          {state.quotaDim && <QuotaRequestDialog owner={owner} dim={state.quotaDim} />}
        </div>
      )}
    </form>
  );
}
