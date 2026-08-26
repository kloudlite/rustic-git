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

export type PullState = { error?: string } | null;

export async function openPull(_prev: PullState, formData: FormData): Promise<PullState> {
  const slug = safeRepoPath(String(formData.get("owner") ?? ""), String(formData.get("repo") ?? ""));
  if (!slug) return { error: "That repository name is not valid." };
  const { owner, repo } = slug;
  const base = String(formData.get("base") ?? "").trim();
  const head = String(formData.get("head") ?? "").trim();
  const title = String(formData.get("title") ?? "").trim();
  const body = String(formData.get("body") ?? "");

  if (!title) return { error: "Give the change a title." };
  if (base === head) return { error: "Pick two different branches." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.openPull(token, owner, repo, { title, body, base, head });
  if (!r.ok) return { error: r.message || "Could not open the change." };
  revalidatePath(`/${owner}/${repo}/pulls`);
  redirect(`/${owner}/${repo}/pulls/${r.value.number}`);
}

export async function comment(_prev: PullState, formData: FormData): Promise<PullState> {
  const slug = safeRepoPath(String(formData.get("owner") ?? ""), String(formData.get("repo") ?? ""));
  if (!slug) return { error: "That repository name is not valid." };
  const { owner, repo } = slug;
  const number = Number(formData.get("number"));
  const body = String(formData.get("body") ?? "").trim();
  if (!body) return { error: "Say something." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.commentOnPull(token, owner, repo, number, body);
  if (!r.ok) return { error: r.message || "Could not post the comment." };
  revalidatePath(`/${owner}/${repo}/pulls/${number}`);
  return null;
}

/** The fleet's refusal is passed through verbatim — "this branch is behind its
 *  base", or the name of the protection rule that stopped it. Both are written
 *  for the person reading them, and a generic message would hide which it was. */
export async function merge(_prev: PullState, formData: FormData): Promise<PullState> {
  const slug = safeRepoPath(String(formData.get("owner") ?? ""), String(formData.get("repo") ?? ""));
  if (!slug) return { error: "That repository name is not valid." };
  const { owner, repo } = slug;
  const number = Number(formData.get("number"));

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const asked = String(formData.get("strategy") ?? "fast-forward");
  const strategy: api.MergeStrategy =
    asked === "squash" || asked === "merge" ? asked : "fast-forward";
  const r = await api.mergePull(token, owner, repo, number, strategy);
  if (!r.ok) return { error: r.message || "Could not merge." };
  revalidatePath(`/${owner}/${repo}`, "layout");
  return null;
}

export async function close(_prev: PullState, formData: FormData): Promise<PullState> {
  const slug = safeRepoPath(String(formData.get("owner") ?? ""), String(formData.get("repo") ?? ""));
  if (!slug) return { error: "That repository name is not valid." };
  const { owner, repo } = slug;
  const number = Number(formData.get("number"));
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  const r = await api.closePull(token, owner, repo, number);
  if (!r.ok) return { error: r.message || "Could not close the change." };
  revalidatePath(`/${owner}/${repo}/pulls/${number}`);
  return null;
}
