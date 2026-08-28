"use server";

import { revalidatePath } from "next/cache";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";

/** Personal settings. Tokens and ssh keys are credentials: nothing here logs a
 *  value, and a token's secret is returned to the browser exactly once — in the
 *  reply to the action that created it, never from a later read. */

export type AddKeyState = { ok?: true; error?: string } | null;

export type DeleteState = { error?: string } | null;

/** Adds an access key, or — with `signing` set — a key that only proves who wrote
 *  a commit. The same key may be added both ways; they grant different things. */
export async function addSshKey(_prev: AddKeyState, formData: FormData): Promise<AddKeyState> {
  const owner = String(formData.get("owner") ?? "").trim();
  const title = String(formData.get("title") ?? "").trim();
  const key = String(formData.get("key") ?? "").trim();
  if (!owner) return { error: "Pick which namespace this key is for." };
  if (!key) return { error: "Paste the public key." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await api.addKey(token, owner, title, key, formData.get("signing") !== null);
  if (!r.ok) {
    if (r.kind === "conflict") return { error: "That key is already added." };
    // The api names what is wrong with a key it could not parse; that message is
    // written for the person, so it is shown rather than replaced.
    if (r.kind === "invalid") return { error: r.message };
    return { error: r.message || "Could not add the key." };
  }
  revalidatePath("/settings");
  return { ok: true };
}

export async function removeSshKey(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const id = String(formData.get("id") ?? "");
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  if (!id) return { error: "No key named." };
  const r = await api.removeKey(token, id);
  if (!r.ok) return { error: r.message || "Could not remove the key." };
  revalidatePath("/settings");
  return null;
}

export async function regeneratePlatformKey(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const owner = String(formData.get("owner") ?? "").trim();
  if (!owner) return { error: "No account named." };
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  const r = await api.regeneratePlatformKey(token, owner);
  if (!r.ok) return { error: r.message || "Could not regenerate the key." };
  revalidatePath("/settings");
  return null;
}

export type CreateTokenState = { token?: string; name?: string; error?: string } | null;

/** Returns the token exactly once, to the form that asked for it. */
export async function createToken(_prev: CreateTokenState, formData: FormData): Promise<CreateTokenState> {
  const owner = String(formData.get("owner") ?? "").trim();
  const name = String(formData.get("name") ?? "").trim();
  if (!owner) return { error: "Pick which namespace this token is for." };
  if (!name) return { error: "Give the token a name." };

  const session = await apiToken();
  if (!session) return { error: "Your session has expired. Sign in again." };

  const r = await api.createToken(session, owner, name);
  if (!r.ok) {
    if (r.kind === "invalid") return { error: r.message };
    return { error: r.message || "Could not create the token." };
  }
  revalidatePath("/settings");
  return { token: r.value.token, name: r.value.name };
}

/** Takes back one CLI login. The row IS the revocation list on the api side, so removing it
 *  stops that token at the next request rather than at its expiry. */
export async function revokeCliToken(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const id = String(formData.get("id") ?? "");
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  if (!id) return { error: "No login named." };
  const r = await api.revokeCliToken(token, id);
  if (!r.ok) return { error: r.message || "Could not revoke the login." };
  revalidatePath("/settings");
  return null;
}

export async function revokeToken(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const id = String(formData.get("id") ?? "");
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  if (!id) return { error: "No token named." };
  const r = await api.revokeToken(token, id);
  if (!r.ok) return { error: r.message || "Could not revoke the token." };
  revalidatePath("/settings");
  return null;
}
