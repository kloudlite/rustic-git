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
export type EnvActionState = { ok?: true; error?: string } | null;

export async function startEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.startEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not start." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

export async function stopEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.stopEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not stop." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

/** New environment grafted onto a past snapshot. The services are the caller's — a snapshot
 *  records the DATA, never a compose file — so an empty list is legal and restores the volume. */
export async function restoreEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const snapshotId = String(formData.get("snapshotId") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the new environment." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.restoreEnvironment(token, name, snapshotId);
  if (!r.ok) return { error: r.message || "Could not restore." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

export async function cloneEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the clone." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.cloneEnvironment(token, id, name);
  if (!r.ok) return { error: r.message || "Could not clone." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

/** Deleting an environment leaves its snapshots alone by default: a snapshot is a point in time
 *  and outlives the thing it was taken of, so the row simply becomes "archived". Checking
 *  "Also delete its snapshots" drops the volume's whole index afterwards — after, so a failed
 *  snapshot delete never leaves an environment that was already removed from the node with a
 *  history nobody can reach. */
export async function deleteEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const alsoSnapshots = formData.get("snapshots") != null;

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  if (alsoSnapshots) {
    // A volume with nothing pushed has no index to drop, and the api answers 404 for that as well
    // as for "not yours" — the environment IS deleted either way, so this is not an error to show.
    await api.deleteVolume(token, id);
  }
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

/** An archived row's own action: the environment is already gone, only its snapshots are left. */
export async function deleteEnvironmentSnapshots(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteVolume(token, id);
  if (!r.ok) return { error: r.message || "Could not delete the snapshots." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}
