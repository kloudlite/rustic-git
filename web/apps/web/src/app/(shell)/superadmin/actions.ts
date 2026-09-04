"use server";

import { revalidatePath } from "next/cache";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { DIMS, type QuotaDim } from "@/lib/quota";
import { conflictMessage } from "@/lib/settings";

/** Bound with (id, decision) as the row's two buttons, so a plain form works with no client
 *  component: the note is whatever the shared field on that row held when it was pressed. */
export async function decideRequest(id: string, decision: "approve" | "deny", formData: FormData) {
  const note = String(formData.get("note") ?? "").trim();
  const token = await tokenOr();
  if (typeof token !== "string") return;
  await api.adminDecideQuotaRequest(id, decision, note, token);
  revalidatePath("/superadmin/requests");
}

/** Bound with the default's own name (`default-user`/`default-team`) — the one writer
 *  `approve_quota_request` also calls, so the queue and this form can never disagree about how a
 *  limit lands. */
export async function writeDefault(owner: string, formData: FormData) {
  const spec = {} as Record<QuotaDim, number>;
  for (const d of DIMS) {
    const n = Number(formData.get(d));
    spec[d] = Number.isFinite(n) && n >= 0 ? n : 0;
  }
  const token = await tokenOr();
  if (typeof token !== "string") return;
  await api.adminWriteQuota(owner, spec, token);
  revalidatePath("/superadmin/owners");
}

export async function createRegionAction(formData: FormData) {
  const id = String(formData.get("id") ?? "").trim();
  const name = String(formData.get("name") ?? "").trim();
  if (!id || !name) return;
  const token = await tokenOr();
  if (typeof token !== "string") return;
  await api.createRegion({ id, name }, token);
  revalidatePath("/superadmin/clusters");
}

export type SaveResult = { ok: true } | { ok: false; message: string };

/** The one write the Clusters and Monitoring tabs offer: a manual restart with a required
 *  reason. `scope` is `"central"` or a region id, same encoding as `WorkloadDoc.scope` — Monitoring
 *  always passes `"central"`, Clusters passes the region being viewed. */
export async function rollWorkloadAction(scope: string, name: string, reason: string): Promise<SaveResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.rollWorkload(scope, name, reason, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath(scope === "central" ? "/superadmin/monitoring" : "/superadmin/clusters");
  return { ok: true };
}
