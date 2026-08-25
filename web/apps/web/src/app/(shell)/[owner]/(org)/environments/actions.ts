"use server";

import { revalidatePath } from "next/cache";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";

export type EnvActionState = { error?: string } | null;

export async function startEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.startEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not start." };
  revalidatePath(`/${owner}/environments`);
  return null;
}

export async function stopEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.stopEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not stop." };
  revalidatePath(`/${owner}/environments`);
  return null;
}

export async function deleteEnvironment(_prev: EnvActionState, formData: FormData): Promise<EnvActionState> {
  const owner = String(formData.get("owner") ?? "");
  const id = String(formData.get("id") ?? "");

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.deleteEnvironment(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  revalidatePath(`/${owner}/environments`);
  return null;
}
