"use server";

import { revalidatePath } from "next/cache";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";
// `owner` reaches every action below as FormData, and goes straight into a revalidatePath
// PATTERN. A segment carrying `/` or `..` would silently revalidate something else, so each
// action refuses it — a bad one is never a real submission, since the pages that render these
// forms fill the field from the route params.
import { safeSegment } from "@/lib/slug";

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

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

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

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.cloneWorkspace(token, id, name);
  if (!r.ok) return { error: r.message || "Could not clone." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function restoreWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const srcWorkspace = String(formData.get("srcWorkspace") ?? "");
  const snapshotId = String(formData.get("snapshotId") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the new workspace." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.restoreWorkspace(token, name, snapshotId, srcWorkspace);
  if (!r.ok) return { error: r.message || "Could not restore." };
  revalidatePath(`/${owner}/workspaces`);
  revalidatePath(`/${owner}/snapshots`);
  return { ok: true };
}

export async function startWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.startWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not start." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function stopWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.stopWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not stop." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function deleteWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}
