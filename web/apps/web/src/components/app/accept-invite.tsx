"use client";

import { useActionState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import type { ApiInvitePreview } from "@/lib/api";
import { accept, type AcceptState } from "@/app/(shell)/invite/[token]/actions";

export function AcceptInvite({ token, preview, me }: { token: string; preview: ApiInvitePreview | null; me: string }) {
  const [state, action, pending] = useActionState<AcceptState, FormData>(accept, null);
  if (!preview) {
    return (
      <div className="max-w-md border border-border bg-card px-6 py-8">
        <h1 className="text-body font-semibold">This invitation is no longer open</h1>
        <p className="mt-2 text-sm2 text-muted-foreground">
          It may have been used already, withdrawn, or expired — invitations last seven days.
          Ask whoever sent it for a new one.
        </p>
      </div>
    );
  }
  const mismatch = preview.email.toLowerCase() !== me.toLowerCase();
  return (
    <div className="max-w-md border border-border bg-card px-6 py-8">
      <h1 className="text-body font-semibold">Join {preview.teamName}</h1>
      <p className="mt-2 text-sm2 text-muted-foreground">
        {preview.invitedBy} invited <span className="font-medium text-foreground">{preview.email}</span> to
        join as <Badge variant="outline" className="capitalize">{preview.role}</Badge>.
      </p>
      {mismatch ? (
        // Said before they click, not after: the api would refuse it anyway.
        <p role="alert" className="mt-4 text-sm2 font-medium text-destructive">
          You are signed in as {me}. Sign in as {preview.email} to accept.
        </p>
      ) : (
        <form action={action} className="mt-6 grid gap-3">
          <input type="hidden" name="invite" value={token} />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <div>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Accept and join</Button>
          </div>
        </form>
      )}
    </div>
  );
}
