"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";

export type SettingsState = { ok?: true; error?: string } | null;

export async function saveDescription(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const repo = String(formData.get("repo") ?? "");
  const description = String(formData.get("description") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.updateRepo(token, owner, repo, { description });
  if (!r.ok) return { error: r.message || "Could not save the description." };
  revalidatePath(`/${owner}/${repo}`, "layout");
  return { ok: true };
}

export async function setVisibility(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const repo = String(formData.get("repo") ?? "");
  const visibility = formData.get("visibility") === "public" ? "public" : "private";

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.updateRepo(token, owner, repo, { visibility });
  if (!r.ok) return { error: r.message || "Could not change visibility." };
  // The badge in the chrome and in every listing reads this, so the whole repo
  // subtree is revalidated rather than just this page.
  revalidatePath(`/${owner}/${repo}`, "layout");
  revalidatePath(`/${owner}`);
  return { ok: true };
}

export async function addRule(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const repo = String(formData.get("repo") ?? "");
  const pattern = String(formData.get("pattern") ?? "").trim();
  if (!pattern) return { error: "Name a branch, or a pattern like release/*." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.setProtection(token, owner, repo, {
    pattern,
    no_force: formData.get("no_force") !== null,
    no_delete: formData.get("no_delete") !== null,
  });
  if (!r.ok) return { error: r.message || "Could not save the rule." };
  revalidatePath(`/${owner}/${repo}/settings`);
  return { ok: true };
}

export async function removeRule(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const repo = String(formData.get("repo") ?? "");
  const pattern = String(formData.get("pattern") ?? "");
  if (!pattern) return { error: "No rule named." };
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  const r = await api.setProtection(token, owner, repo, { pattern, remove: true });
  if (!r.ok) return { error: r.message || "Could not remove the rule." };
  revalidatePath(`/${owner}/${repo}/settings`);
  return null;
}

/** Deleting is irreversible and there is no undo behind it, so the form makes the
 *  person type the repo's name and this checks it again — a disabled button is a
 *  hint, not a gate. */
export async function destroyRepo(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const repo = String(formData.get("repo") ?? "");
  const confirm = String(formData.get("confirm") ?? "").trim();
  if (confirm !== repo) return { error: `Type ${repo} exactly to confirm.` };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteRepo(token, owner, repo);
  if (!r.ok) return { error: r.message || "Could not delete the repository." };
  revalidatePath(`/${owner}`);
  redirect(`/${owner}`);
}
