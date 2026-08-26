"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";
// `owner` and `repo` reach every action below as FormData, and go straight into a
// revalidatePath PATTERN. A segment carrying `/` or `..` would silently revalidate something
// else, so each action refuses it — a bad one is never a real submission, since the pages that
// render these forms fill both fields from the route params.
import { safeRepoPath } from "@/lib/slug";
import { pathHref } from "@/lib/utils";

export type EditState = { error?: string } | null;

/** Commit an edited file.
 *
 *  The choice of where it lands is the form's: onto the branch being viewed, or
 *  onto a new one so it can be reviewed first. Everything else -- who the author
 *  is, whether the branch moved, whether protection allows it -- is decided by
 *  the server, which is why none of it is sent from here. */
export async function commitFile(_prev: EditState, formData: FormData): Promise<EditState> {
  const slug = safeRepoPath(String(formData.get("owner") ?? ""), String(formData.get("repo") ?? ""));
  if (!slug) return { error: "That repository name is not valid." };
  const { owner, repo } = slug;
  const branch = String(formData.get("branch") ?? "");
  const path = String(formData.get("path") ?? "");
  const expect = String(formData.get("expect") ?? "") || undefined;
  const content = String(formData.get("content") ?? "");
  const target = String(formData.get("target") ?? "here");
  const newBranch = String(formData.get("newBranch") ?? "").trim();

  const message =
    String(formData.get("message") ?? "").trim() || `Update ${path.split("/").pop() ?? path}`;

  if (target === "branch" && !newBranch) return { error: "Name the new branch." };
  if (target === "branch" && newBranch === branch) {
    return { error: "That is the branch you are already on." };
  }

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  // A textarea gives back a JS string; the server wants the file's bytes. UTF-8
  // first, so anything outside Latin-1 survives the trip.
  const contentBase64 = Buffer.from(content, "utf8").toString("base64");

  const r = await api.commitPatch(token, owner, repo, {
    branch,
    message,
    expect,
    newBranch: target === "branch" ? newBranch : undefined,
    changes: [{ path, contentBase64 }],
  });
  if (!r.ok) return { error: r.message || "Could not commit." };

  const landed = r.value.branch;
  revalidatePath(`/${owner}/${repo}`, "layout");
  // Onto a new branch, the next thing anyone wants is the pull request -- that is
  // why they chose a branch rather than committing here.
  if (target === "branch") {
    redirect(`/${owner}/${repo}/pulls/new?base=${encodeURIComponent(branch)}&head=${encodeURIComponent(landed)}`);
  }
  redirect(`/${owner}/${repo}/blob/${pathHref(path)}?ref=${encodeURIComponent(landed)}`);
}
