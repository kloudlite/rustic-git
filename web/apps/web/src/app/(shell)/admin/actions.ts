"use server";

import { revalidatePath } from "next/cache";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { DIMS, type QuotaDim } from "@/lib/quota";

/** Bound with (id, decision) as the row's two buttons, so a plain form works with no client
 *  component: the note is whatever the shared field on that row held when it was pressed. */
export async function decideRequest(id: string, decision: "approve" | "deny", formData: FormData) {
  const note = String(formData.get("note") ?? "").trim();
  const token = await tokenOr();
  if (typeof token !== "string") return;
  await api.adminDecideQuotaRequest(id, decision, note, token);
  revalidatePath("/admin");
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
  revalidatePath("/admin/defaults");
}

export async function createRegionAction(formData: FormData) {
  const id = String(formData.get("id") ?? "").trim();
  const name = String(formData.get("name") ?? "").trim();
  if (!id || !name) return;
  const token = await tokenOr();
  if (typeof token !== "string") return;
  await api.createRegion({ id, name }, token);
  revalidatePath("/admin/regions");
}
