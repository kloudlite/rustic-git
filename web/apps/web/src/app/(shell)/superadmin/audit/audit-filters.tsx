import Link from "next/link";
import { Input } from "@/components/ui/input";
import type { AuditFilter } from "@/lib/audit";

/** A native GET form: submitting it is a browser navigation to `?actor=&action=&…`, which is all
 *  the filtering this page needs — the server component above re-reads `searchParams` and
 *  re-fetches. No client JS, no state to keep in sync with the URL by hand. */
export function AuditFilters({ filter, knownActions }: { filter: AuditFilter; knownActions: string[] }) {
  return (
    <form method="get" className="mb-3 flex flex-wrap items-end gap-2">
      <label className="flex flex-col gap-1 text-caption text-muted-foreground">
        Actor
        <Input name="actor" defaultValue={filter.actor ?? ""} placeholder="op@example.com" className="h-8 w-44 text-sm2" />
      </label>
      <label className="flex flex-col gap-1 text-caption text-muted-foreground">
        Action
        {/* Free text, exact match — the backend's action words aren't a fixed enum the web owns
         *  (more land with later tasks), so a hard-coded <select> silently drifts stale. The
         *  datalist offers what's actually in the rows already on screen, never invents one. */}
        <Input
          name="action"
          list="audit-known-actions"
          defaultValue={filter.action ?? ""}
          placeholder="any action"
          className="h-8 w-40 text-sm2"
        />
        <datalist id="audit-known-actions">
          {knownActions.map((a) => (
            <option key={a} value={a} />
          ))}
        </datalist>
      </label>
      <label className="flex flex-col gap-1 text-caption text-muted-foreground">
        Target
        <Input name="target" defaultValue={filter.target ?? ""} placeholder="acme" className="h-8 w-36 text-sm2" />
      </label>
      <label className="flex flex-col gap-1 text-caption text-muted-foreground">
        From
        <Input type="date" name="from" defaultValue={filter.from ?? ""} className="h-8 w-36 text-sm2" />
      </label>
      <label className="flex flex-col gap-1 text-caption text-muted-foreground">
        To
        <Input type="date" name="to" defaultValue={filter.to ?? ""} className="h-8 w-36 text-sm2" />
      </label>
      <button type="submit" className="h-8 border border-border px-3 text-sm2 hover:bg-muted">
        Filter
      </button>
      <Link href="/superadmin/audit" className="h-8 px-2 text-sm2 leading-8 text-muted-foreground hover:underline">
        Clear
      </Link>
    </form>
  );
}
