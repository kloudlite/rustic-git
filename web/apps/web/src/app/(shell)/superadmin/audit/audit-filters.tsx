import Link from "next/link";
import { Input } from "@/components/ui/input";
import { AUDIT_ACTIONS, type AuditFilter } from "@/lib/audit";

/** A native GET form: submitting it is a browser navigation to `?actor=&action=&…`, which is all
 *  the filtering this page needs — the server component above re-reads `searchParams` and
 *  re-fetches. No client JS, no state to keep in sync with the URL by hand. */
export function AuditFilters({ filter }: { filter: AuditFilter }) {
  return (
    <form method="get" className="mb-3 flex flex-wrap items-end gap-2">
      <label className="flex flex-col gap-1 text-caption text-muted-foreground">
        Actor
        <Input name="actor" defaultValue={filter.actor ?? ""} placeholder="op@example.com" className="h-8 w-44 text-sm2" />
      </label>
      <label className="flex flex-col gap-1 text-caption text-muted-foreground">
        Action
        {/* A plain <select name="action"> rather than the styled combobox: the combobox has no
         *  form-submittable value without client JS, and this form is deliberately a native GET. */}
        <select
          name="action"
          defaultValue={filter.action ?? ""}
          className="h-8 w-40 border border-border bg-background px-2 text-sm2"
        >
          <option value="">any action</option>
          {AUDIT_ACTIONS.map((a) => (
            <option key={a} value={a}>{a}</option>
          ))}
        </select>
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
