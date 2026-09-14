"use client";

import { useActionState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { stopUsingForMySpace, useForMySpace, type EnvActionState } from "@/app/(shell)/[owner]/(org)/environments/actions";

/** The one control for which environment your space follows: every workspace and the bench you
 *  have under this owner resolve its services by bare name, running ones included. Three states —
 *  not in use, in use, and the refusal the api gave. */
export function SpaceEnvControl({ owner, id, inUse }: { owner: string; id: string; inUse: boolean }) {
  const [chooseState, choose, choosePending] = useActionState<EnvActionState, FormData>(useForMySpace, null);
  const [stopState, stop, stopPending] = useActionState<EnvActionState, FormData>(stopUsingForMySpace, null);
  const error = (inUse ? stopState : chooseState)?.error;
  return (
    <span className="flex items-center gap-2">
      {inUse ? (
        <form action={stop} className="flex items-center gap-2">
          <span className="text-caption text-muted-foreground">In use by your workspaces</span>
          <input type="hidden" name="owner" value={owner} />
          <Button type="submit" variant="outline" size="sm" disabled={stopPending}>
            {stopPending && <Loader2 className="animate-spin" />}Stop using
          </Button>
        </form>
      ) : (
        <form action={choose}>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <Button type="submit" variant="outline" size="sm" disabled={choosePending}>
            {choosePending && <Loader2 className="animate-spin" />}Use for my workspaces
          </Button>
        </form>
      )}
      {error && <span role="alert" className="text-caption font-medium text-destructive">{error}</span>}
    </span>
  );
}
