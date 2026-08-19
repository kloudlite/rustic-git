"use server";

import { revalidatePath } from "next/cache";

/** Personal settings. Real server actions so the forms already post the right way;
 *  the bodies are what changes when the API client lands. Tokens and keys are
 *  credentials, so nothing here logs a value. */
export async function updateProfile(formData: FormData) {
  void formData.get("name");
  revalidatePath("/settings");
}

export async function addSshKey(formData: FormData) {
  void formData.get("title");
  void formData.get("key");
  revalidatePath("/settings");
}

export async function removeSshKey(formData: FormData) {
  void formData.get("id");
  revalidatePath("/settings");
}

export async function createToken(formData: FormData) {
  void formData.get("name");
  void formData.getAll("scope");
  void formData.get("expires");
  revalidatePath("/settings");
}

export async function revokeToken(formData: FormData) {
  void formData.get("id");
  revalidatePath("/settings");
}
