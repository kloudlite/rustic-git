"use server";

import { revalidatePath } from "next/cache";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
// `owner` and `id` reach every action below as FormData, and go straight into a revalidatePath
// PATTERN. A segment carrying `/` or `..` would silently revalidate something else, so each
// action refuses it — a bad one is never a real submission, since the pages that render these
// forms fill the field from the route params.
import { safeSegment } from "@/lib/slug";

/** `ok` is what lets a dialog close on success — see `useDialogUntilSuccess`. */
export type EnvActionState = {
  ok?: true;
  error?: string;
  /** A stop off a dead node: edits since the last sync point stay there. Shown, not fatal. */
  warning?: string;
  /** A push's request id — the only thing `push` answers with. See `pushEnvironment`. */
  requestId?: string;
} | null;

export async function startEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That environment is not valid." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.startEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not start." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

export async function stopEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.stopEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not stop." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true, warning: r.value?.warning };
}

export async function pushEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That environment is not valid." };
  const message = String(formData.get("message") ?? "").trim();

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.pushEnvironment(token, id, message || undefined);
  if (!r.ok) return { error: r.message || "Could not push." };
  revalidatePath(`/${owner}/environments/${id}/snapshots`);
  return { ok: true, requestId: r.value.id };
}

/** The Restore dialog's one action, for both shapes it takes.
 *
 *  `mode` says which: `inplace` puts the snapshot back into THIS environment's own volume (202
 *  and nothing to read — the controllers scale the services down, swap the subvolume and bring
 *  them back up); `new` restores it into a fresh environment under `name`. The dialog states the
 *  choice outright rather than the action inferring it from a name, because two client-supplied
 *  names that happen to match are not a decision.
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
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That environment is not valid." };
  const snapshotId = String(formData.get("snapshotId") ?? "");
  // Empty when the dialog's snapshot list has not landed, or landed empty. The disabled submit
  // button is a convenience; this is the check.
  if (!snapshotId) return { error: "Choose a snapshot to restore." };
  const mode = formData.get("mode");
  if (mode !== "inplace" && mode !== "new") return { error: "Could not tell where to restore to." };
  const name = String(formData.get("name") ?? "").trim();
  if (mode === "new" && !name) return { error: "Name the environment to restore into." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

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
    mode === "inplace"
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
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That environment is not valid." };
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the clone." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.cloneEnvironment(token, id, name);
  if (!r.ok) return { error: r.message || "Could not clone." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

/** Deleting an environment always leaves its snapshots: a snapshot is a point in time, outlives
 *  the thing it was taken of, and is kept until it is explicitly deleted. The environment then
 *  appears under Snapshots, which is where deleting them for good lives — one destructive choice
 *  in one place, rather than a checkbox riding along with a delete that is otherwise recoverable. */
export async function deleteEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}

/** One snapshot out of the lineage — the explicit delete a snapshot is kept until. The live
 *  environment's own disk is not touched. */
export async function deleteEnvironmentSnapshot(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That environment is not valid." };
  const snapshotId = String(formData.get("snapshotId") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteVolumeSnapshot(token, id, snapshotId);
  if (!r.ok) return { error: r.message || "Could not delete the snapshot." };
  revalidatePath(`/${owner}/environments/${id}/snapshots`);
  return { ok: true };
}

/** The Snapshots section's own delete: the environment is already gone, and its snapshots are the
 *  only thing keeping its volume. `deleteWorkspaceSnapshots` is the same action for the other kind. */
export async function deleteEnvironmentSnapshots(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That environment is not valid." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteVolume(token, id);
  if (!r.ok) return { error: r.message || "Could not delete the snapshots." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}
