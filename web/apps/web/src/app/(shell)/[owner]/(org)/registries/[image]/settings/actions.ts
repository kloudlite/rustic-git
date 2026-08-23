"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { apiToken } from "@/lib/api-token";
import { deleteImage, deleteImageTag } from "@/lib/browse";

export type SettingsState = { ok?: true; error?: string } | null;

/** One tag, gone. The manifest it pointed at is left alone — see
 *  `deleteImageTag`'s own doc comment — so this never touches a sibling tag on
 *  the same manifest. */
export async function removeTag(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const image = String(formData.get("image") ?? "");
  const tag = String(formData.get("tag") ?? "");
  if (!tag) return { error: "No tag named." };
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  const r = await deleteImageTag(token, owner, image, tag);
  if (!r.ok) return { error: r.message || "Could not delete the tag." };
  revalidatePath(`/${owner}/registries/${image}`, "layout");
  return null;
}

/** Deleting is irreversible, so the form makes the person type the image's name
 *  and this checks it again — the same pattern `destroyRepo` uses, a disabled
 *  button is a hint, not a gate. */
export async function destroyImage(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const image = String(formData.get("image") ?? "");
  const confirm = String(formData.get("confirm") ?? "").trim();
  if (confirm !== image) return { error: `Type ${image} exactly to confirm.` };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await deleteImage(token, owner, image);
  if (!r.ok) return { error: r.message || "Could not delete the image." };
  revalidatePath(`/${owner}/registries`);
  redirect(`/${owner}/registries`);
}
