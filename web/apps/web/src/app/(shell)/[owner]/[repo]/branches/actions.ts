"use server";

import { revalidatePath } from "next/cache";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
// Same reasoning as the settings actions: `owner` and `repo` arrive as FormData and go
// straight into a revalidatePath pattern, so a segment carrying `/` or `..` is refused.
import { safeRepoPath } from "@/lib/slug";
import type { DeleteState } from "@/components/app/delete-form";

/** Delete one branch. `oid` is what the page listed, and the fleet deletes only if the
 *  branch is still there — a branch that moved answers 409 with its own sentence, which
 *  is shown verbatim rather than replaced by a friendlier lie. */
export async function deleteBranch(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const slug = safeRepoPath(String(formData.get("owner") ?? ""), String(formData.get("repo") ?? ""));
  if (!slug) return { error: "That repository name is not valid." };
  const { owner, repo } = slug;
  const branch = String(formData.get("branch") ?? "");
  const oid = String(formData.get("oid") ?? "");
  if (!branch || !oid) return { error: "No branch named." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteBranch(token, owner, repo, branch, oid);
  if (!r.ok) return { error: r.message || "Could not delete the branch." };
  revalidatePath(`/${owner}/${repo}/branches`);
  // The Code tab's branch picker is rendered by the repo root, which would keep offering a branch
  // that no longer exists until its own cache entry expired.
  revalidatePath(`/${owner}/${repo}`);
  return null;
}
