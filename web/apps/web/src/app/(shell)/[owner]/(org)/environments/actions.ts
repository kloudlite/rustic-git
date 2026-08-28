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
export type EnvActionState = {
  ok?: true;
  error?: string;
  /** A push's request id — the only thing `push` answers with. See `pushEnvironment`. */
  requestId?: string;
} | null;

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

export async function pushEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const message = String(formData.get("message") ?? "").trim();

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.pushEnvironment(token, id, message || undefined);
  if (!r.ok) return { error: r.message || "Could not push." };
  revalidatePath(`/${owner}/environments/${id}/snapshots`);
  return { ok: true, requestId: r.value.id };
}

/** Put a past snapshot back into THIS environment's own volume. 202 and nothing to read: the
 *  controllers scale the services down, swap the subvolume and bring them back up, and the
 *  environment's own state is where that shows. */
export async function restoreEnvironmentInPlace(
  _prev: EnvActionState,
  formData: FormData,
): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const snapshotId = String(formData.get("snapshotId") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.restoreEnvironmentInPlace(token, id, snapshotId);
  if (!r.ok) return { error: r.message || "Could not restore." };
  revalidatePath(`/${owner}/environments/${id}`);
  revalidatePath(`/${owner}/environments/${id}/snapshots`);
  return { ok: true };
}

/** The Restore dialog's one action, for both shapes it takes.
 *
 *  Same name as the environment restores IN PLACE (the volume it already has); a different name
 *  restores into a NEW environment. That is the whole rule the dialog states, in the one place
 *  that can enforce it — a second action would let the two drift.
 *
 *  `snapshotFirst` pushes the current state before restoring and WAITS for the record to land:
 *  the offer is "so you can come back to it", and a restore that started before its safety
 *  snapshot was durable would have made that a lie. `push` answers with a request id, not a
 *  record, so landing is only observable as the record appearing in the volume's history.
 *  ponytail: a 60 s ceiling polled every 2 s — a multi-gigabyte first push can outlast it, and the
 *  restore is then refused rather than run; following the SnapshotRequest's own status is the fix
 *  once `/v1` projects one by id. */
export async function restoreEnvironmentFrom(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const snapshotId = String(formData.get("snapshotId") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  const currentName = String(formData.get("currentName") ?? "").trim();
  if (!name) return { error: "Name the environment to restore into." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  if (formData.get("snapshotFirst") != null) {
    const before = await api.volumeHistory(token, id);
    const had = before.ok ? before.value.length : 0;
    const message = String(formData.get("snapshotMessage") ?? "").trim() || `before restore to ${snapshotId.slice(0, 8)}`;
    const push = await api.pushEnvironment(token, id, message);
    if (!push.ok) return { error: push.message || "Could not take the snapshot; nothing was restored." };
    let landed = false;
    for (let i = 0; i < 30 && !landed; i++) {
      await new Promise((r) => setTimeout(r, 2_000));
      const now = await api.volumeHistory(token, id);
      landed = now.ok && now.value.length > had;
    }
    if (!landed) return { error: "The snapshot has not landed yet. Nothing was restored — try again in a moment." };
  }

  const r =
    name === currentName
      ? await api.restoreEnvironmentInPlace(token, id, snapshotId)
      : await api.restoreEnvironment(token, name, snapshotId);
  if (!r.ok) return { error: r.message || "Could not restore." };
  revalidatePath(`/${owner}/environments`);
  revalidatePath(`/${owner}/environments/${id}`);
  revalidatePath(`/${owner}/environments/${id}/snapshots`);
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

/** One record out of the lineage. The disk is not touched: what goes is the environment's record
 *  of that snapshot, which is why the dialog says so rather than warning about data loss. */
export async function deleteEnvironmentSnapshot(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const snapshotId = String(formData.get("snapshotId") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteVolumeSnapshot(token, id, snapshotId);
  if (!r.ok) return { error: r.message || "Could not delete the snapshot." };
  revalidatePath(`/${owner}/environments/${id}/snapshots`);
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
