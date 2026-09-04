import type { SettingsSchemaRow } from "@/lib/api";

/** The "takes effect" column. A `boot` field is the dangerous one — saving it ROLLS its readers
 *  (CLAUDE.md, live settings) — so the column names them rather than saying "boot" and leaving an
 *  operator to guess which pods restart. `mark` is lowercase on the wire
 *  (`crates/workspaces/src/api/admin/schema.rs`). */
export function takesEffect(row: SettingsSchemaRow): string {
  if (row.mark !== "boot") return "live";
  return row.readers.length ? `boot: rolls ${row.readers.join(", ")}` : "boot";
}

export function matchesSearch(row: SettingsSchemaRow, q: string): boolean {
  const needle = q.trim().toLowerCase();
  if (!needle) return true;
  return row.name.toLowerCase().includes(needle) || row.description.toLowerCase().includes(needle);
}
