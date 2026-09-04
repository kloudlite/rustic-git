/** The one shape `/admin/audit` and `/admin/audit.csv` both take (`crates/workspaces/src/api/admin/audit.rs`). */
export type AuditFilter = {
  actor?: string;
  action?: string;
  target?: string;
  /** `yyyy-mm` or `yyyy-mm-dd`. */
  from?: string;
  /** `yyyy-mm` or `yyyy-mm-dd`. */
  to?: string;
  cursor?: string;
  limit?: number;
};

export type AuditEntry = {
  ts: string;
  actor: string;
  action: string;
  target: string;
  reason?: string | null;
  result: string;
};

export type AuditPage = {
  rows: AuditEntry[];
  next_cursor?: string | null;
};

// Fixed order so two calls with the same filter produce byte-identical query strings — that's
// what makes `auditQueryString` testable without normalizing anything.
const FIELD_ORDER = ["actor", "action", "target", "from", "to", "cursor", "limit"] as const;

/** URL-encodes only the fields that are actually set, in a stable order. Used both by the page's
 *  fetch and by the CSV export link, so the two never drift apart on how a filter becomes a URL. */
export function auditQueryString(filter: AuditFilter): string {
  const q = new URLSearchParams();
  for (const key of FIELD_ORDER) {
    const v = filter[key];
    if (v === undefined || v === "") continue;
    q.set(key, String(v));
  }
  const qs = q.toString();
  return qs ? `?${qs}` : "";
}

// The write actions named in the plan's vocabulary (constraints.md) — Task 1 owns the backend's
// canonical list; this is the UI's filter choices, not an API field, so it's fine to keep in sync
// by hand rather than fetch a schema for a fixed, small set.
export const AUDIT_ACTIONS = [
  "approve",
  "deny",
  "set_quota",
  "roll",
  "drain",
  "undrain",
  "decommission",
  "add_superadmin",
  "remove_superadmin",
  "create_region",
] as const;
