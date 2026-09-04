"use server";

import { revalidatePath } from "next/cache";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { DIMS, type QuotaDim } from "@/lib/quota";
import { conflictMessage } from "@/lib/settings";
import type { AuditFilter, AuditPage } from "@/lib/audit";

export type DecideResult = { ok: true } | { ok: false; message: string };

/** The decision panel's Approve/Deny. `requested` is the operator's edited grant, one input per
 *  dimension the request touched — Deny never reads it, only the note. A 409 (someone else
 *  already decided this one) surfaces to the panel rather than a toast, so the operator sees the
 *  conflicting state next to the row they were acting on. */
export async function decideRequest(
  id: string,
  decision: "approve" | "deny",
  note: string,
  requested: Partial<Record<QuotaDim, number>> | undefined,
): Promise<DecideResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminDecideQuotaRequest(id, decision, note, token, requested);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath("/superadmin/requests");
  return { ok: true };
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

export type AuditPageResult = { ok: true; page: AuditPage } | { ok: false; message: string };

/** The "Load more" button's fetch: same filter as the page's initial server-rendered load, just
 *  with the previous page's `next_cursor`. A read, but a server action all the same — the browser
 *  never gets the admin token, only this. */
export async function loadMoreAudit(filter: AuditFilter, cursor: string): Promise<AuditPageResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminAudit(token, { ...filter, cursor });
  if (!r.ok) return { ok: false, message: r.message };
  return { ok: true, page: r.value };
}
