"use client";

import { useActionState } from "react";
import { CheckCircle2, Loader2, Terminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import { approveCliCode, type ApproveState } from "./actions";

/** The one button on the page. Denying is leaving — nothing is written until Approve is
 *  pressed, and an unapproved code expires on its own, so a Deny button would only be a
 *  slower way of closing the tab. */
export function Approve({ code, device }: { code: string; device: string }) {
  const [state, action, pending] = useActionState<ApproveState, FormData>(approveCliCode, null);

  if (state?.ok) {
    return (
      <div className="mt-6 flex items-start gap-3 border border-border bg-card px-5 py-4">
        <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-success" />
        <div>
          <p className="text-sm2 font-medium">Approved — return to your terminal</p>
          <p className="mt-1 text-sm2 text-muted-foreground">
            You can close this tab. Manage this login under Profile settings → CLI logins.
          </p>
        </div>
      </div>
    );
  }

  return (
    <form action={action} className="mt-6 grid gap-4">
      <div className="flex items-center gap-3 border border-border bg-card px-5 py-4">
        <Terminal className="size-4 shrink-0 text-muted-foreground" />
        <div className="min-w-0">
          {/* The device name comes from an unauthenticated request, so it is rendered as text —
              JSX escapes it — and truncated rather than allowed to reshape the page. */}
          <p className="truncate text-sm2 font-medium">{device}</p>
          <code className="font-mono text-caption text-muted-foreground">{code}</code>
        </div>
      </div>
      <p className="text-sm2 text-muted-foreground">
        Approve only if that is your own machine and the code matches the one in its terminal.
        Approving signs it in as you.
      </p>
      <input type="hidden" name="code" value={code} />
      {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
      <div>
        <Button type="submit" disabled={pending}>
          {pending && <Loader2 className="animate-spin" />}Approve
        </Button>
      </div>
    </form>
  );
}
