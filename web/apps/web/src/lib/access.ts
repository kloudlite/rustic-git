import type { SuperAdmin } from "./api";

/** Why Remove is disabled for this row, or null when it's allowed — the same two rules the api
 *  enforces (`crates/api/src/teams.rs`'s `is_same_user`/`is_last_superadmin`), mirrored here so
 *  the button's tooltip can name the reason before a click ever reaches the server. */
export function removeDisabledReason(row: SuperAdmin, rows: SuperAdmin[], selfEmail: string): string | null {
  if (row._id.trim().toLowerCase() === selfEmail.trim().toLowerCase()) {
    return "You cannot remove your own administrator claim";
  }
  if (rows.length === 1) {
    return "The last administrator cannot be removed";
  }
  return null;
}
