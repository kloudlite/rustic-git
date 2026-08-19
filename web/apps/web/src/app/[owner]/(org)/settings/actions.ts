"use server";

import { revalidatePath } from "next/cache";

/** Persisting team settings belongs to the API client, which does not exist yet.
 *  The actions are real server actions so the forms already post the right way;
 *  the bodies are the only thing that changes when the client lands. */
export async function updateTeam(formData: FormData) {
  void formData.get("name");
  void formData.get("description");
  revalidatePath("/[owner]/settings", "page");
}

export async function inviteMember(formData: FormData) {
  void formData.get("email");
  void formData.get("role");
  revalidatePath("/[owner]/settings", "page");
}
