"use client";

import { useMemo, useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import { Loader2, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import type { ApiEnvironment, ApiWorkspace } from "@/lib/api";
import { Section } from "../../ui/section";
import { DataTable, EmptyState, RowActions, Td, Th, Tr } from "../../ui/data-table";
import {
  adminDeleteEnvironmentAction, adminDeleteWorkspaceAction, adminStopEnvironmentAction, adminStopWorkspaceAction,
} from "../../actions";

/** One live working copy's Stop/Delete — the same admin routes an owner's own workspaces page
 *  uses, just cross-owner, and therefore behind a confirmation that names the consequence and
 *  takes the required reason the audit row carries. No age column: neither `ApiWorkspace` nor
 *  `ApiEnvironment` carries a creation timestamp today (see `crates/workspaces/src/model.rs`),
 *  and inventing one here would be a field the api never sent. */
type Row =
  | { kind: "workspace"; id: string; name: string; state: ApiWorkspace["state"]; node: string | null; region: string }
  | { kind: "environment"; id: string; name: string; state: ApiEnvironment["state"]; node: string | null; region: string };

function ActionCell({ owner, row }: { owner: string; row: Row }) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [note, setNote] = useState("");
  const [pending, startTransition] = useTransition();
  const [error, setError] = useState<string | null>(null);
  const running = row.state === "running" || row.state === "ready";
  const verb = running ? "Stop" : "Delete";

  function act() {
    if (note.trim() === "") return;
    setError(null);
    startTransition(async () => {
      const action = row.kind === "workspace"
        ? (running ? adminStopWorkspaceAction : adminDeleteWorkspaceAction)
        : (running ? adminStopEnvironmentAction : adminDeleteEnvironmentAction);
      const r = await action(owner, row.id, note.trim());
      if (!r.ok) {
        setError(r.message);
        return;
      }
      setOpen(false);
      router.refresh();
    });
  }

  return (
    <>
      <button
        type="button"
        className={running ? "text-sm2 text-muted-foreground hover:text-primary" : "text-sm2 text-muted-foreground hover:text-destructive"}
        onClick={() => {
          setOpen(true);
          setNote("");
          setError(null);
        }}
      >
        {verb}
      </button>
      <Dialog open={open} onOpenChange={(o) => !o && setOpen(false)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{verb} {row.name}</DialogTitle>
            <DialogDescription>
              {running
                ? `This ${row.kind} belongs to ${owner} and is running right now — stopping it interrupts whoever is using it.`
                : `Deleting this ${row.kind} removes ${owner}'s working copy. Pushed snapshots are kept.`}
            </DialogDescription>
          </DialogHeader>
          <Input
            value={note}
            onChange={(e) => setNote(e.target.value)}
            placeholder="Why (required)"
            aria-label="Note"
          />
          {error && <p role="alert" className="text-sm2 font-medium text-destructive">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="button" onClick={act} disabled={pending || note.trim() === ""}>
              {pending && <Loader2 className="animate-spin" />}
              {verb}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
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
  const [q, setQ] = useState("");
  const rows: Row[] = useMemo(
    () => [
      ...workspaces.map((w): Row => ({ kind: "workspace", id: w.id, name: w.name, state: w.state, node: w.placement, region: w.region })),
      ...environments.map((e): Row => ({ kind: "environment", id: e.id, name: e.name, state: e.state, node: e.placement, region: e.region })),
    ],
    [workspaces, environments],
  );
  const needle = q.trim().toLowerCase();
  const shown = needle ? rows.filter((r) => r.name.toLowerCase().includes(needle)) : rows;

  return (
    <Section
      eyebrow="Allocation"
      title="Live workspaces and environments"
      count={rows.length}
      bare
      toolbar={
        <div className="relative w-56">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Filter by name"
            aria-label="Filter by name"
            className="h-8 pl-8 text-sm2"
          />
        </div>
      }
    >
      {shown.length === 0 ? (
        <EmptyState>No workspace or environment is live for this owner.</EmptyState>
      ) : (
        <DataTable>
          <thead>
            <tr>
              <Th>Name</Th>
              <Th>Kind</Th>
              <Th>State</Th>
              <Th>Node</Th>
              <Th>Region</Th>
              <Th />
            </tr>
          </thead>
          <tbody>
            {shown.map((r) => (
              <Tr key={`${r.kind}-${r.id}`}>
                <Td className="font-mono text-caption">{r.name}</Td>
                <Td className="text-muted-foreground">{r.kind}</Td>
                <Td><WsEnvStateBadge state={r.state} /></Td>
                <Td className="font-mono text-caption text-muted-foreground">{r.node ?? "unplaced"}</Td>
                <Td className="text-muted-foreground">{r.region}</Td>
                <Td>
                  <RowActions>
                    {(r.state === "running" || r.state === "ready" || r.state === "stopped" || r.state === "error") && (
                      <ActionCell owner={owner} row={r} />
                    )}
                  </RowActions>
                </Td>
              </Tr>
            ))}
          </tbody>
        </DataTable>
      )}
    </Section>
  );
}
