"use server";

import { revalidatePath } from "next/cache";

/** Personal settings. Real server actions so the forms already post the right way;
 *  the bodies are what changes when the API client lands. Tokens and keys are
 *  credentials, so nothing here logs a value. */
export async function updateProfile(formData: FormData) {
  void formData.get("name");
  revalidatePath("/settings");
}

export type AddKeyState = { ok?: true; error?: string } | null;

export async function addSshKey(_prev: AddKeyState, formData: FormData): Promise<AddKeyState> {
  const title = String(formData.get("title") ?? "").trim();
  const key = String(formData.get("key") ?? "").trim();
  if (!title) return { error: "Give the key a title." };
  if (!/^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp256) /.test(key)) return { error: "That does not look like a public key. It should start with ssh-ed25519 or ssh-rsa." };
  revalidatePath("/settings");
  return { ok: true };
}

export async function removeSshKey(formData: FormData) {
  void formData.get("id");
  revalidatePath("/settings");
}

export type CreateTokenState = { token?: string; name?: string; error?: string } | null;

/** Returns the token exactly once, to the form that asked for it. The list is
 *  revalidated so the new entry appears behind the dialog. */
export async function createToken(_prev: CreateTokenState, formData: FormData): Promise<CreateTokenState> {
  const name = String(formData.get("name") ?? "").trim();
  const scopes = formData.getAll("scope").map(String);
  void formData.get("expires");
  if (!name) return { error: "Give the token a name." };
  if (scopes.length === 0) return { error: "Pick at least one scope." };
  // Mock: a token shaped like the real one. The API mints and returns the value.
  const token = "klp_" + Array.from({ length: 40 }, () => "abcdefghijklmnopqrstuvwxyz0123456789"[Math.floor(Math.random() * 36)]).join("");
  revalidatePath("/settings");
  return { token, name };
}

export async function revokeToken(formData: FormData) {
  void formData.get("id");
  revalidatePath("/settings");
}
