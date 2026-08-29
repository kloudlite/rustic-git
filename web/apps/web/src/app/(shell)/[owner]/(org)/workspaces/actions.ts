"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
// `owner` reaches every action below as FormData, and goes straight into a revalidatePath
// PATTERN. A segment carrying `/` or `..` would silently revalidate something else, so each
// action refuses it — a bad one is never a real submission, since the pages that render these
// forms fill the field from the route params.
import { safeSegment } from "@/lib/slug";
import { getSession } from "@/lib/session";

/** `ok` is what lets a dialog close on success — see `useDialogUntilSuccess`. */
export type WsActionState = { ok?: true; error?: string } | null;

/** Mutations are async jobs (202 + a doc whose `state` is still `creating`), so
 *  there is nothing to poll here: revalidating just re-renders the list with
 *  whatever state the api already wrote, same as every other list in the app. */
export async function pushWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const message = String(formData.get("message") ?? "").trim();

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.pushWorkspace(token, id, message || undefined);
  if (!r.ok) return { error: r.message || "Could not push." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function cloneWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the clone." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.cloneWorkspace(token, id, name);
  if (!r.ok) return { error: r.message || "Could not clone." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function restoreWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const snapshotId = String(formData.get("snapshotId") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the new workspace." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.restoreWorkspace(token, name, snapshotId);
  if (!r.ok) return { error: r.message || "Could not restore." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function startWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.startWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not start." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function stopWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.stopWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not stop." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function deleteWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

/** "Open in a workspace", from the repo Clone menu and the PR header: one workspace per
 *  (repo, branch), reused if it is already there. The backend does the rest — the controller
 *  clones the repo with a token minted for this caller. */
export async function openInWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  const repo = safeSegment(String(formData.get("repo") ?? ""));
  // A branch is not a path segment — it legitimately carries `/` — so it gets its own rule
  // rather than `safeSegment`. It never reaches revalidatePath; it only goes to the api.
  const branch = String(formData.get("branch") ?? "").trim();
  if (!owner || !repo) return { error: "That repository name is not valid." };
  if (!branch || branch.includes("..") || branch.startsWith("-")) return { error: "That branch name is not valid." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const session = await getSession();

  // A repo under your own handle is personal work, not a team's — same rule the api applies.
  const team = session?.user.owner === owner ? undefined : owner;
  const name = `${repo}-${branch}`
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 40);

  const existing = await api.listWorkspaces(token, team);
  if (!existing.ok) return { error: existing.message || "Could not read your workspaces." };
  if (!existing.value.some((w) => w.name === name)) {
    const regions = await api.listRegions(token);
    if (!regions.ok) return { error: regions.message || "Could not read the regions." };
    // ponytail: first ACTIVE region; a picker when there is a second. A retired region stays in
    // the list so its old records still resolve, so "first" alone once chose a region with no
    // agents in it and the workspace sat unplaced forever.
    const region = regions.value.find((r) => r.status === "active")?.id;
    if (!region) return { error: "No region is available to run a workspace in." };

    const r = await api.createWorkspace(token, {
      team,
      name,
      region,
      quota_gb: 10,
      repo: `${owner}/${repo}`,
      branch,
    });
    if (!r.ok) return { error: r.message || "Could not open a workspace." };
  }

  revalidatePath(`/${owner}/workspaces`);
  // Outside every catch above on purpose: redirect works by throwing.
  redirect(`/${owner}/workspaces`);
}

/** The whole package list, replaced. The field is free text (whitespace or commas) because that
 *  is how the names are written down everywhere else — a nixpkgs attribute never contains
 *  either, so the split cannot corrupt one. Validation is the api's; its 422 names the entry. */
export async function setPackages(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const packages = String(formData.get("packages") ?? "")
    .split(/[\s,]+/)
    .filter(Boolean);

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.setWorkspacePackages(token, id, packages);
  if (!r.ok) return { error: r.message || "Could not set the packages." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}
