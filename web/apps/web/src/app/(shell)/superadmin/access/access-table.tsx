"use client";

import { useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { when } from "@/lib/time";
import type { SuperAdmin } from "@/lib/api";
import { removeDisabledReason } from "@/lib/access";
import { addSuperadminAction, removeSuperadminAction } from "../actions";
import { Section } from "../ui/section";
import { DataTable, EmptyState, RowActions, Td, Th, Tr } from "../ui/data-table";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { Pill } from "../ui/pill";

// The bootstrap path (`Directory::bootstrap_superadmins`, `crates/pulls/src/directory/mod.rs`)
// writes this literal as `addedBy` — nothing else does, so it doubles as the origin condition.
const BOOTSTRAP = "bootstrap";

function AddPanel({ onDone }: { onDone: () => void }) {
  const [email, setEmail] = useState("");
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  function submit() {
    if (email.trim() === "" || note.trim() === "") return;
    startTransition(async () => {
      const r = await addSuperadminAction(email.trim(), note.trim());
      if (!r.ok) {
        setError(r.message);
        return;
      }
      onDone();
    });
  }

  return (
    <Section eyebrow="Access" title="Add a superadmin" toolbar={<span className="text-caption text-muted-foreground">Takes effect at their next sign-in</span>}>
      <div className="flex flex-col gap-3">
        <label className="flex flex-col gap-1 text-caption text-muted-foreground">
          Email
          <Input value={email} onChange={(e) => setEmail(e.target.value)} type="email" placeholder="name@kloudlite.io" className="sm:max-w-xs" />
        </label>
        <label className="flex flex-col gap-1 text-caption text-muted-foreground">
          Note
          <Textarea value={note} onChange={(e) => setNote(e.target.value)} rows={2} placeholder="Required — why this person needs it" />
        </label>
        {error && <p className="text-caption text-destructive">{error}</p>}
        <div className="flex gap-2">
          <Button type="button" onClick={submit} disabled={pending || email.trim() === "" || note.trim() === ""}>
            {pending && <Loader2 className="animate-spin" />}
            Add superadmin
          </Button>
          <Button type="button" variant="outline" onClick={onDone}>Cancel</Button>
        </div>
      </div>
    </Section>
  );
}

function RemovePanel({ email, onDone }: { email: string; onDone: () => void }) {
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  function submit() {
    if (note.trim() === "") return;
    startTransition(async () => {
      const r = await removeSuperadminAction(email, note.trim());
      if (!r.ok) {
        setError(r.message);
        return;
      }
      onDone();
    });
  }

  return (
    <Section
      eyebrow="Access"
      title={`Remove ${email}`}
      toolbar={<Pill tone="critical">confirmation required</Pill>}
    >
      <div className="flex flex-col gap-3">
        {/* The consequence spelled out before the note, not after: this is the one write on the
            page that takes something away, and the panel replaces v1's two-step dialog. */}
        <p className="text-sm2 text-muted-foreground">
          They lose the superadmin claim at their next sign-in, and every session they hold now is
          refused on its next admin call. Their pending approvals stay pending for someone else to decide.
        </p>
        <label className="flex flex-col gap-1 text-caption text-muted-foreground">
          Note
          <Textarea value={note} onChange={(e) => setNote(e.target.value)} rows={2} placeholder="Required — recorded in the audit log" />
        </label>
        {error && <p className="text-caption text-destructive">{error}</p>}
        <div className="flex gap-2">
          <Button
            type="button"
            variant="outline"
            className="border-destructive/40 text-destructive hover:bg-destructive/10"
            onClick={submit}
            disabled={pending || note.trim() === ""}
          >
            {pending && <Loader2 className="animate-spin" />}
            Remove superadmin
          </Button>
          <Button type="button" variant="outline" onClick={onDone}>Cancel</Button>
        </div>
      </div>
    </Section>
  );
}

export function AccessTable({ rows, selfEmail }: { rows: SuperAdmin[]; selfEmail: string }) {
  const router = useRouter();
  const [q, setQ] = useState("");
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);

  const shown = rows.filter((r) => r._id.toLowerCase().includes(q.trim().toLowerCase()));
  const bootstrapped = rows.filter((r) => r.addedBy === BOOTSTRAP).length;
  const month = new Date().toISOString().slice(0, 7);
  const thisMonth = rows.filter((r) => r.addedAt.startsWith(month) && r.addedBy !== BOOTSTRAP);

  function done() {
    setAdding(false);
    setRemoving(null);
    router.refresh();
  }

  return (
    <div className="space-y-4">
      <KpiStrip>
        <KpiTile
          label="Superadmins"
          value={rows.length}
          sub={`${bootstrapped} bootstrapped · ${rows.length - bootstrapped} added since`}
        />
        <KpiTile
          label="Added this month"
          value={thisMonth.length}
          sub={thisMonth[0] ? `${thisMonth[0]._id}, added by ${thisMonth[0].addedBy}` : "nobody this month"}
        />
      </KpiStrip>

      <Section
        eyebrow="Access"
        title="Superadmins"
        count={rows.length}
        bare
        toolbar={
          <>
            <Input className="h-8 w-56" placeholder="Filter by email" value={q} onChange={(e) => setQ(e.target.value)} />
            <button
              type="button"
              className="h-8 shrink-0 border border-border px-3 text-sm2 hover:bg-muted"
              onClick={() => setAdding(true)}
            >
              Add superadmin
            </button>
          </>
        }
      >
        {shown.length === 0 ? (
          <EmptyState>No superadmin matches that. Clear the filter to see the whole list.</EmptyState>
        ) : (
          <DataTable>
            <thead>
              <tr>
                <Th>Email</Th>
                <Th>Added by</Th>
                <Th numeric>Added at</Th>
                <Th>Origin</Th>
                <Th />
              </tr>
            </thead>
            <tbody>
              {shown.map((r) => {
                const disabled = removeDisabledReason(r, rows, selfEmail);
                return (
                  <Tr key={r._id}>
                    <Td className="font-medium">{r._id}</Td>
                    <Td className="text-muted-foreground">{r.addedBy}</Td>
                    <Td numeric><span title={r.addedAt}>{when(new Date(r.addedAt).getTime())}</span></Td>
                    <Td>
                      {r.addedBy === BOOTSTRAP ? <Pill>bootstrap</Pill> : <span className="text-muted-foreground">—</span>}
                    </Td>
                    <Td>
                      <RowActions>
                        <button
                          type="button"
                          className="text-sm2 text-muted-foreground hover:text-destructive disabled:opacity-50 disabled:hover:text-muted-foreground"
                          disabled={disabled !== null}
                          title={disabled ?? undefined}
                          onClick={() => setRemoving(r._id)}
                        >
                          Remove
                        </button>
                      </RowActions>
                    </Td>
                  </Tr>
                );
              })}
            </tbody>
          </DataTable>
        )}
        <p className="border-t border-border px-4 py-2 text-caption text-muted-foreground">
          The bootstrap list only seeds this collection at boot — removing an address from{" "}
          <span className="font-mono">KLOUDLITE_WORKSPACES_ADMINS</span> revokes nobody. Remove them here.
        </p>
      </Section>

      {adding && <AddPanel onDone={done} />}
      {removing && <RemovePanel email={removing} onDone={done} />}
    </div>
  );
}
