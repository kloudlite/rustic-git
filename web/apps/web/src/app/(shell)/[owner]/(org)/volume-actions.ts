"use server";

import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import type { ApiCommitRecord } from "@/lib/api";

/** A volume's snapshots, read from a client dialog when it OPENS rather than with the listing
 *  that renders the row.
 *
 *  Deliberately lazy: the Snapshots section is a list of things you are not looking at, and one
 *  history read per row is a request per deleted workspace on every page load. Only the row whose
 *  Restore dialog is opened pays for one. The api scopes the lookup to volumes the caller may
 *  read, so this shares its authorization rather than adding any. */
export async function volumeSnapshots(
  name: string,
): Promise<{ ok: true; rows: ApiCommitRecord[] } | { ok: false; error: string }> {
  const token = await tokenOr();
  // An expired session is NOT "no snapshots" — see `lib/require-api.ts`. Collapsing either
  // failure into `[]` told someone their last copy was gone, silently.
  if (typeof token !== "string") return { ok: false, error: token.error };
  const r = await api.volumeHistory(token, name);
  return r.ok ? { ok: true, rows: r.value } : { ok: false, error: r.message || "Could not read the snapshots." };
}
