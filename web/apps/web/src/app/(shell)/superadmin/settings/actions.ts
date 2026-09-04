"use server";

import { revalidatePath } from "next/cache";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { conflictMessage } from "@/lib/settings";

/** Called directly from `SettingsTable` (a client component), not bound to a `<form>`: the diff
 *  is an arbitrary field set the table already holds as state, so there is nothing a `FormData`
 *  encoding buys over passing the object straight through — server actions are plain callable
 *  functions, not only form targets. */
export type SaveResult = { ok: true } | { ok: false; message: string };

export async function saveCentralSettings(patch: Record<string, unknown>): Promise<SaveResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.putCentralSettings(patch, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath("/superadmin/settings");
  return { ok: true };
}

export async function saveClusterSettings(region: string, patch: Record<string, unknown>): Promise<SaveResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.putClusterSettings(region, patch, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath("/superadmin/settings/clusters");
  return { ok: true };
}

/** One depth only — the most recent prior version (`n = 0`), the one thing a revert button needs. */
export async function revertClusterSettingsAction(region: string): Promise<SaveResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.revertClusterSettings(region, 0, token);
  if (!r.ok) return { ok: false, message: r.message };
  revalidatePath("/superadmin/settings/clusters");
  return { ok: true };
}

/** Central's own single-depth revert (`POST /admin/settings/central/revert`, unblocked by the
 *  Rust fix at `fd9e851a`) — same shape as the cluster one above. */
export async function revertCentralSettingsAction(): Promise<SaveResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.revertCentralSettings(token);
  if (!r.ok) return { ok: false, message: r.message };
  revalidatePath("/superadmin/settings");
  return { ok: true };
}

/** The Workloads tab's one write: a manual roll with an operator-typed reason. `scope` is
 *  `"central"` or a region id (`WorkloadDoc.scope`'s own encoding). */
export async function rollWorkloadAction(scope: string, name: string, reason: string): Promise<SaveResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.rollWorkload(scope, name, reason, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath("/superadmin/settings/workloads");
  return { ok: true };
}
