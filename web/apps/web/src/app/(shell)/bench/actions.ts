"use server";

import { tokenOr } from "@/lib/api-token";
import { benchSession } from "@/lib/api";
import { termUrl } from "@/lib/term-url";

export type BenchSessionResult = { error: string } | { state: string } | { termUrl: string };

/** Called by the page on every render (and every `AutoRefresh` tick while waking): mints a fresh
 *  60s bench-session token each time, never cached — the ponytail comment in `term.rs` is why.
 *  `id` (the caller's own bench) comes back from the api, not from the browser. */
export async function getBenchSession(): Promise<BenchSessionResult> {
  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await benchSession(token);
  if (!r.ok) return { error: r.message || "Could not reach your bench." };
  if ("state" in r.value) return { state: r.value.state };

  return { termUrl: termUrl(r.value.gateway, r.value.id, r.value.token) };
}
