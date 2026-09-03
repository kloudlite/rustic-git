"use server";

import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { DIMS, type QuotaDim } from "@/lib/quota";

export type QuotaRequestState = { ok?: true; error?: string } | null;

/** One raise per dimension the person actually typed a number into — an untouched field asks for
 *  nothing, rather than a silent "raise to 0". */
export async function requestQuota(_prev: QuotaRequestState, formData: FormData): Promise<QuotaRequestState> {
  const owner = String(formData.get("owner") ?? "").trim();
  const reason = String(formData.get("reason") ?? "").trim();
  if (!reason) return { error: "Say what the extra room is for." };

  const requested: Partial<Record<QuotaDim, number>> = {};
  for (const d of DIMS) {
    const raw = String(formData.get(d) ?? "").trim();
    if (!raw) continue;
    const n = Number(raw);
    if (!Number.isFinite(n) || n < 0) return { error: `That is not a valid amount for ${d}.` };
    requested[d] = n;
  }
  if (Object.keys(requested).length === 0) return { error: "Raise at least one dimension." };

  const token = await tokenOr();
  if (typeof token !== "string") return { error: token.error };

  const r = await api.createQuotaRequest({ owner: owner || undefined, requested, reason }, token);
  if (!r.ok) {
    if (r.kind === "conflict") return { error: "A request is already pending for this owner." };
    if (r.kind === "forbidden") return { error: "Only a team admin can request a team quota." };
    return { error: r.message || "Could not send the request." };
  }
  return { ok: true };
}
