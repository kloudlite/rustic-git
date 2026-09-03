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
export async function volumeSnapshots(name: string): Promise<ApiCommitRecord[]> {
  const token = await tokenOr();
  if (typeof token !== "string") return [];
  const r = await api.volumeHistory(token, name);
  return r.ok ? r.value : [];
}
