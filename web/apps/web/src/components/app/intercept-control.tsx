"use client";

import { useActionState, useState } from "react";
import { Loader2, Split } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { DeleteForm } from "@/components/app/delete-form";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { releaseIntercept, setIntercept, type EnvActionState } from "@/app/(shell)/[owner]/(org)/environments/actions";

/** The workspaces the dialog offers, narrowed by the page to the ones that could serve. */
export type InterceptCandidate = { id: string; name: string };

/** Ask for an intercept: one workspace, and the port on it that answers each port the service
 *  declares — defaulting to the service's own number, which is the mapping the api applies when
 *  an entry is left out.
 *
 *  The list is the viewer's own workspaces in this environment's region, not "the attached ones":
 *  the workspace document carries no attachment, so the api — which does — is what decides, and
 *  it refuses an unattached one with a sentence naming the problem. */
export function InterceptDialog({
  owner,
  id,
  service,
  ports,
  workspaces,
}: {
  owner: string;
  id: string;
  service: string;
  ports: number[];
  workspaces: InterceptCandidate[];
}) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(setIntercept, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  const [workspace, setWorkspace] = useState(workspaces[0]?.id ?? "");
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" disabled={workspaces.length === 0}><Split />Intercept</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Intercept <span className="font-mono">{service}</span></DialogTitle>
            <DialogDescription>
              Everything in this environment that dials <span className="font-mono">{service}</span> reaches
              your workspace instead. The real service is stopped while it holds, and comes back
              when you release.
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <input type="hidden" name="service" value={service} />
          <div className="grid gap-1">
            <FieldLabel htmlFor={`int-ws-${service}`}>Workspace</FieldLabel>
            {/* Through a hidden input: this is a server action, and a Radix select has no `name`. */}
            <input type="hidden" name="workspace" value={workspace} />
            <Select value={workspace} onValueChange={setWorkspace}>
              <SelectTrigger id={`int-ws-${service}`} className="h-9">
                <SelectValue placeholder="Choose a workspace" />
              </SelectTrigger>
              <SelectContent>
                {workspaces.map((w) => (
                  <SelectItem key={w.id} value={w.id}>{w.name}</SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-caption text-muted-foreground">
              It has to be running and attached to this environment.
            </p>
          </div>
          {ports.length > 0 && (
            <div className="grid gap-2">
              <FieldLabel htmlFor={`int-port-${service}-${ports[0]}`}>Ports</FieldLabel>
              {ports.map((p) => (
                <div key={p} className="flex items-center gap-2 text-sm2">
                  <span className="font-mono text-muted-foreground">{service}:{p}</span>
                  <span className="text-muted-foreground" aria-hidden>&rarr;</span>
                  <span className="text-muted-foreground">workspace port</span>
                  <Input
                    id={`int-port-${service}-${p}`}
                    name={`port.${p}`}
                    type="number"
                    min={1}
                    max={65535}
                    defaultValue={p}
                    aria-label={`Workspace port answering ${service}:${p}`}
                    className="h-9 w-28 font-mono"
                  />
                </div>
              ))}
              <p className="text-caption text-muted-foreground">
                Callers keep dialling {service}&rsquo;s own port; this is where your workspace listens.
              </p>
            </div>
          )}
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending || !workspace}>
              {pending && <Loader2 className="animate-spin" />}Intercept
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The only thing that drops the wish, so it asks first — the browser's own question, the same
 *  one every other action with nothing to fill in uses. Offered whether or not the intercept is
 *  in force: a wish waiting on a stopped workspace is exactly what someone comes here to undo. */
export function ReleaseIntercept({ owner, id, service }: { owner: string; id: string; service: string }) {
  return (
    <DeleteForm
      action={releaseIntercept}
      fields={{ owner, id, service }}
      confirm={`Release the intercept on ${service}? The real service starts again.`}
    >
      <Button type="submit" variant="outline" size="sm">Release</Button>
    </DeleteForm>
  );
}
