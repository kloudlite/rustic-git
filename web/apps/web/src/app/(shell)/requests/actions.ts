"use server";

import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { blockFor, KINDS, type RequestKind } from "@/lib/requests";

export type NewRequestState = { ok?: true; error?: string } | null;

export async function newRequest(_prev: NewRequestState, formData: FormData): Promise<NewRequestState> {
  const kind = String(formData.get("kind") ?? "") as RequestKind;
  if (!(KINDS as readonly string[]).includes(kind)) return { error: "Pick what you are asking for." };
  const owner = String(formData.get("owner") ?? "").trim();
  const reason = String(formData.get("reason") ?? "").trim();
  if (!reason) return { error: "Say what this is for." };

  let block: Record<string, unknown>;
  try {
    block = blockFor(kind, formData);
  } catch (e) {
    return { error: e instanceof Error ? e.message : "That form is not complete." };
  }

  const token = await tokenOr();
  if (typeof token !== "string") return { error: token.error };

  const r = await api.createRequest({ owner: owner || undefined, kind, reason, ...block }, token);
  if (!r.ok) {
    if (r.kind === "conflict") return { error: "A request of this kind is already pending." };
    if (r.kind === "forbidden") return { error: "Only a team admin can ask on a team's behalf." };
    return { error: r.message || "Could not send the request." };
  }
  return { ok: true };
}
