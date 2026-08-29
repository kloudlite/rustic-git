"use server";

import { redirect } from "next/navigation";
import { updateSession } from "@/auth";
import { tokenOr } from "@/lib/api-token";
import { claimUsername } from "@/lib/api";

export type ClaimState = { error?: string; suggestion?: string } | null;

/** Claim a handle. The api server decides — every rule, and whether it is taken —
 *  so this only shapes the answer for the form. */
export async function claim(_prev: ClaimState, formData: FormData): Promise<ClaimState> {
  const username = String(formData.get("username") ?? "").trim().toLowerCase();
  if (!username) return { error: "Pick a handle." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await claimUsername(token, username);
  if (!r.ok) {
    if (r.kind === "conflict") return { error: `${username} is taken.`, suggestion: `${username}-1` };
    return { error: r.message || "That handle cannot be used." };
  }

  /* The token changed — it now carries the handle — so the session is updated
     server-side before anything routes on it. Deliberately NOT passed through a
     redirect: a bearer token in a URL lands in browser history, the Referer
     header and every access log between here and there. */
  await updateSession({
    apiToken: r.value.token ?? undefined,
    user: { username: r.value.user.username },
  } as never);
  redirect(`/${r.value.user.username}`);
}
