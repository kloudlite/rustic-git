"use server";

import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";

export type ApproveState = { ok?: true; error?: string } | null;

/** Approves a device code as the signed-in person. Nothing is returned to the browser: the
 *  token this mints goes to the waiting CLI over its own poll, never through this page. */
export async function approveCliCode(_prev: ApproveState, formData: FormData): Promise<ApproveState> {
  const code = String(formData.get("code") ?? "").trim();
  if (!code) return { error: "No code to approve." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.approveCliCode(token, code);
  if (!r.ok) {
    // The api answers 404 for unknown, expired and already-approved alike, so this
    // sentence has to cover all three — it must not confirm that some other code exists.
    if (r.kind === "notFound") return { error: "That code is no longer waiting. Run kl login again for a fresh one." };
    if (r.kind === "conflict") return { error: "That code has already been approved." };
    if (r.kind === "invalid") return { error: r.message };
    return { error: r.message || "Could not approve this login." };
  }
  return { ok: true };
}
