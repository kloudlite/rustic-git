"use client";

import { useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import type { ApiEnvironment, ApiWorkspace } from "@/lib/api";
import {
  adminDeleteEnvironmentAction, adminDeleteWorkspaceAction, adminStopEnvironmentAction, adminStopWorkspaceAction,
} from "../../actions";

type Row =
  | { kind: "workspace"; id: string; name: string; state: ApiWorkspace["state"]; node: string | null; region: string }
  | { kind: "environment"; id: string; name: string; state: ApiEnvironment["state"]; node: string | null; region: string };

/** One live working copy's Stop/Delete — the same admin routes an owner's own workspaces page
 *  uses, just cross-owner. No age column: neither `ApiWorkspace` nor `ApiEnvironment` carries a
 *  creation timestamp today (see `crates/workspaces/src/model.rs`), and inventing one here would
 *  be a field the api never sent. */
function ActionCell({ owner, row }: { owner: string; row: Row }) {
  const router = useRouter();
  const [pending, startTransition] = useTransition();
  const [error, setError] = useState<string | null>(null);
  const running = row.state === "running" || row.state === "ready";

  function act() {
    setError(null);
    startTransition(async () => {
      const action = row.kind === "workspace"
        ? (running ? adminStopWorkspaceAction : adminDeleteWorkspaceAction)
        : (running ? adminStopEnvironmentAction : adminDeleteEnvironmentAction);
      const r = await action(owner, row.id);
      if (!r.ok) {
        setError(r.message);
        return;
      }
      router.refresh();
    });
  }

  return (
    <div className="flex items-center justify-end gap-2">
      {error && <span className="text-caption text-destructive">{error}</span>}
      <Button type="button" size="sm" variant="outline" onClick={act} disabled={pending} className={running ? "" : "text-destructive"}>
        {pending && <Loader2 className="animate-spin" />}
        {running ? "Stop" : "Delete"}
      </Button>
    </div>
  );
}

export function LiveObjects({
  owner,
  workspaces,
  environments,
}: {
  owner: string;
  workspaces: ApiWorkspace[];
  environments: ApiEnvironment[];
}) {
  const rows: Row[] = [
    ...workspaces.map((w): Row => ({ kind: "workspace", id: w.id, name: w.name, state: w.state, node: w.placement, region: w.region })),
    ...environments.map((e): Row => ({ kind: "environment", id: e.id, name: e.name, state: e.state, node: e.placement, region: e.region })),
  ];

  return (
    <div className="border border-border bg-card p-4">
      <div className="mb-3 flex items-center justify-between">
        <span className="text-sm2 font-medium">Live working copies · {rows.length}</span>
      </div>
      {rows.length === 0 ? (
        <p className="text-sm2 text-muted-foreground">No workspace or environment is live for this owner.</p>
      ) : (
        <table className="w-full text-left text-sm2">
          <thead>
            <tr className="border-b border-border text-caption text-muted-foreground">
              <th className="py-2 pr-3 font-medium">Name</th>
              <th className="py-2 pr-3 font-medium">Kind</th>
              <th className="py-2 pr-3 font-medium">State</th>
              <th className="py-2 pr-3 font-medium">Node</th>
              <th className="py-2 pr-3 font-medium">Region</th>
              <th className="py-2 font-medium" />
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={`${r.kind}-${r.id}`} className="border-b border-border last:border-0">
                <td className="py-2 pr-3 font-mono text-caption">{r.name}</td>
                <td className="py-2 pr-3 text-muted-foreground">{r.kind}</td>
                <td className="py-2 pr-3"><WsEnvStateBadge state={r.state} /></td>
                <td className="py-2 pr-3 font-mono text-caption text-muted-foreground">{r.node ?? "unplaced"}</td>
                <td className="py-2 pr-3 text-muted-foreground">{r.region}</td>
                <td className="py-2">
                  {(r.state === "running" || r.state === "ready" || r.state === "stopped" || r.state === "error") && (
                    <ActionCell owner={owner} row={r} />
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
