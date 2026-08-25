"use server";

import { revalidatePath } from "next/cache";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";

export type WsActionState = { error?: string } | null;

/** Mutations are async jobs (202 + a doc whose `state` is still `creating`), so
 *  there is nothing to poll here: revalidating just re-renders the list with
 *  whatever state the api already wrote, same as every other list in the app. */
export async function pushWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");
  const message = String(formData.get("message") ?? "").trim();

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.pushWorkspace(token, id, message || undefined);
  if (!r.ok) return { error: r.message || "Could not push." };
  revalidatePath(`/${owner}/workspaces`);
  return null;
}

export async function cloneWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the clone." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.cloneWorkspace(token, id, name);
  if (!r.ok) return { error: r.message || "Could not clone." };
  revalidatePath(`/${owner}/workspaces`);
  return null;
}

export async function restoreWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = String(formData.get("owner") ?? "");
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
  return null;
}

export async function startWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.startWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not start." };
  revalidatePath(`/${owner}/workspaces`);
  return null;
}

export async function stopWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.stopWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not stop." };
  revalidatePath(`/${owner}/workspaces`);
  return null;
}

export async function deleteWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  revalidatePath(`/${owner}/workspaces`);
  return null;
}
