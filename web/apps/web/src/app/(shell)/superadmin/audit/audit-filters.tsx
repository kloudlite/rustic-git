import Link from "next/link";
import { Input } from "@/components/ui/input";
import type { AuditFilter } from "@/lib/audit";

/** A native GET form: submitting it is a browser navigation to `?actor=&action=&…`, which is all
 *  the filtering this page needs — the server component above re-reads `searchParams` and
 *  re-fetches. No client JS, no state to keep in sync with the URL by hand.
 *
 *  It renders inside the section header (design mockup), so the fields are labelled by
 *  `aria-label` rather than a stacked `<label>` that would double the header's height. */
export function AuditFilters({ filter, knownActions }: { filter: AuditFilter; knownActions: string[] }) {
  return (
    <form method="get" className="flex flex-wrap items-center gap-2">
      <Input
        name="actor"
        aria-label="Actor"
        defaultValue={filter.actor ?? ""}
        placeholder="Any actor"
        className="h-8 w-36 text-sm2"
      />
      {/* Free text, exact match — the backend's action words aren't a fixed enum the web owns, so
       *  a hard-coded <select> silently drifts stale. The datalist offers what's actually in the
       *  rows already on screen, never invents one. */}
      <Input
        name="action"
        aria-label="Action"
        list="audit-known-actions"
        defaultValue={filter.action ?? ""}
        placeholder="Any action"
        className="h-8 w-36 text-sm2"
      />
      <datalist id="audit-known-actions">
        {knownActions.map((a) => (
          <option key={a} value={a} />
        ))}
      </datalist>
      <Input
        name="target"
        aria-label="Target"
        defaultValue={filter.target ?? ""}
        placeholder="Any target"
        className="h-8 w-32 text-sm2"
      />
      <Input type="date" name="from" aria-label="From" defaultValue={filter.from ?? ""} className="h-8 w-36 text-sm2" />
      <Input type="date" name="to" aria-label="To" defaultValue={filter.to ?? ""} className="h-8 w-36 text-sm2" />
      <button type="submit" className="h-8 border border-border px-3 text-sm2 hover:bg-muted">
        Filter
      </button>
      <Link href="/superadmin/audit" className="h-8 px-2 text-sm2 leading-8 text-muted-foreground hover:underline">
        Clear
      </Link>
    </form>
  );
}
