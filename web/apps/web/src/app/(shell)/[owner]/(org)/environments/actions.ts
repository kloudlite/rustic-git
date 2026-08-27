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

export async function deleteEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  revalidatePath(`/${owner}/environments`);
  return { ok: true };
}
