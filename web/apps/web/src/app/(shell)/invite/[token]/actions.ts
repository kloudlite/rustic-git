"use server";

import { redirect } from "next/navigation";
import { apiToken } from "@/lib/api-token";
import { acceptInvite } from "@/lib/api";

export type AcceptState = { error?: string } | null;

export async function accept(_prev: AcceptState, formData: FormData): Promise<AcceptState> {
  const token = String(formData.get("invite") ?? "");
  if (!token) return { error: "That link is not valid." };
  const session = await apiToken();
  if (!session) return { error: "Your session has expired. Sign in again." };
  const r = await acceptInvite(session, token);
  // 403 is "sent to a different email", 404 is spent or expired — the api's words are the
  // right ones for both, since the fix is on the person's side.
  if (!r.ok) return { error: r.message || "Could not accept the invitation." };
  redirect(`/${r.value.team}`);
}
