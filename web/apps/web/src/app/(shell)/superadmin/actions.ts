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

export type QuotaWriteResult = { ok: true } | { ok: false; message: string };

function specFrom(formData: FormData): Record<QuotaDim, number> {
  const spec = {} as Record<QuotaDim, number>;
  for (const d of DIMS) {
    const n = Number(formData.get(d));
    spec[d] = Number.isFinite(n) && n >= 0 ? n : 0;
  }
  return spec;
}

/** Bound with the default's own name (`default-user`/`default-team`) — the one writer
 *  `approve_quota_request` also calls, so the queue and this form can never disagree about how a
 *  limit lands. `note` is required (a quota write is a dangerous one, per the Global Constraint);
 *  the api 422s an empty one and that message surfaces to the form rather than being swallowed. */
export async function writeDefault(owner: string, formData: FormData): Promise<QuotaWriteResult> {
  const note = String(formData.get("note") ?? "").trim();
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminWriteQuota(owner, specFrom(formData), note, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath("/superadmin/owners");
  return { ok: true };
}

/** The owner detail page's Set quota form — same writer, any owner rather than a default name. */
export async function setQuota(owner: string, formData: FormData): Promise<QuotaWriteResult> {
  const note = String(formData.get("note") ?? "").trim();
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminWriteQuota(owner, specFrom(formData), note, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath(`/superadmin/owners/${encodeURIComponent(owner)}`);
  return { ok: true };
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

export type WriteResult = { ok: true } | { ok: false; message: string };

/** The owner detail page's Stop/Delete on a live working copy — same routes an owner's own
 *  workspaces page uses, just cross-owner. `owner` names the detail page to revalidate. */
export async function adminStopWorkspaceAction(owner: string, id: string): Promise<WriteResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminStopWorkspace(id, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath(`/superadmin/owners/${encodeURIComponent(owner)}`);
  return { ok: true };
}

export async function adminDeleteWorkspaceAction(owner: string, id: string): Promise<WriteResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminDeleteWorkspace(id, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath(`/superadmin/owners/${encodeURIComponent(owner)}`);
  return { ok: true };
}

export async function adminStopEnvironmentAction(owner: string, id: string): Promise<WriteResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminStopEnvironment(id, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath(`/superadmin/owners/${encodeURIComponent(owner)}`);
  return { ok: true };
}

export async function adminDeleteEnvironmentAction(owner: string, id: string): Promise<WriteResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return { ok: false, message: token.error };
  const r = await api.adminDeleteEnvironment(id, token);
  if (!r.ok) return { ok: false, message: r.kind === "conflict" ? conflictMessage(r.message) : r.message };
  revalidatePath(`/superadmin/owners/${encodeURIComponent(owner)}`);
  return { ok: true };
}
