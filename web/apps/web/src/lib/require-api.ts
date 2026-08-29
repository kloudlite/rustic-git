import "server-only";
import { redirect } from "next/navigation";
import type { ApiResult } from "@/lib/api";

/**
 * Unwrap a list call, or send the person to sign in.
 *
 * `unauthorized` means the api rejected our token — the session outlived it.
 * That is emphatically NOT "you have no keys" or "no such repo", and rendering
 * it as either is how a page comes to claim someone's data has vanished. It has
 * happened: an expired token made every list empty and every repo look deleted,
 * with nothing logged, because a failed call was quietly turned into `[]`.
 */
export function listOrSignIn<T>(r: ApiResult<T[]>): T[] {
  if (r.ok) return r.value;
  if (r.kind === "unauthorized") redirect("/login?from=expired");
  // Anything else degrades to empty: one unreachable list is not worth an error
  // page over the rest of a settings screen.
  return [];
}
