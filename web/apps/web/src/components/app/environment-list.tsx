"use client";

import { useActionState, useMemo, useState } from "react";
import { Layers, Loader2, Play, Search, Square } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import type { ApiEnvironment } from "@/lib/api";
import { startEnvironment, stopEnvironment, type EnvActionState } from "@/app/(shell)/[owner]/(org)/environments/actions";

/** Start/stop, same bare-form idiom as `pull-actions.tsx`: one hidden id, no
 *  dialog, since neither action takes a value from the person. */
function ToggleForm({ owner, id, running }: { owner: string; id: string; running: boolean }) {
  const action = running ? stopEnvironment : startEnvironment;
  const [state, act, pending] = useActionState<EnvActionState, FormData>(action, null);
  return (
    <form action={act} className="contents">
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending} title={state?.error}>
        {pending ? <Loader2 className="animate-spin" /> : running ? <Square /> : <Play />}
        {running ? "Stop" : "Start"}
      </Button>
    </form>
  );
}

/** Same filter idiom as `repo-list.tsx` and `workspace-list.tsx`. */
export function EnvironmentList({ owner, environments }: { owner: string; environments: ApiEnvironment[] }) {
  const [q, setQ] = useState("");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return environments;
    return environments.filter((e) => e.name.toLowerCase().includes(needle));
  }, [environments, q]);

  if (environments.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Layers className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">No environments yet</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          An environment runs one or more services, each backed by a volume.
        </p>
      </div>
    );
  }

  return (
    <>
      <div className="relative w-full max-w-xs">
        <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
        <Input
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Filter environments"
          aria-label="Filter environments"
          className="h-8 pl-8 text-sm2"
        />
      </div>

      {shown.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {shown.map((e) => (
            <li key={e.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-2.5">
                  <span className="truncate text-body font-medium">{e.name}</span>
                  <WsEnvStateBadge state={e.state} />
                </span>
                <span className="mt-1 block text-sm2 text-muted-foreground">
                  {e.region} · {e.services.length} {e.services.length === 1 ? "service" : "services"}
                </span>
              </span>
              <div className="flex shrink-0 items-center gap-2">
                <ToggleForm owner={owner} id={e.id} running={e.state === "running"} />
              </div>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
