"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";
import { safeSegment } from "@/lib/slug";

/** Team settings. The api authorizes every one of these on the team's members —
 *  the slug in the form says which team, never whether the caller may touch it.
 *  Nothing here decides membership; a refusal comes back as the api's own words. */

export type TeamState = { ok?: true; error?: string } | null;

/** The slug goes into a revalidatePath pattern, so a bad one is refused rather
 *  than revalidating something else. A real submission never carries one: the
 *  page fills the field from the route. */
function slugOf(formData: FormData): string | null {
  return safeSegment(String(formData.get("slug") ?? ""));
}

async function tokenOr(): Promise<string | TeamState> {
  return (await apiToken()) ?? { error: "Your session has expired. Sign in again." };
}

export async function saveTeam(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const name = String(formData.get("name") ?? "").trim();
  const description = String(formData.get("description") ?? "");
  if (!name) return { error: "Give the team a name." };
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const r = await api.updateTeam(token, slug, { name, description });
  if (!r.ok) return { error: r.message || "Could not save." };
  // `layout`: the name is in the switcher and the shell header, not just this page.
  revalidatePath(`/${slug}`, "layout");
  return { ok: true };
}

export async function addMember(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const email = String(formData.get("email") ?? "").trim();
  const role = formData.get("role") === "admin" ? "admin" : "member";
  if (!email) return { error: "Enter their email." };
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const r = await api.addTeamMember(token, slug, email, role);
  if (!r.ok) {
    if (r.kind === "conflict") return { error: "They are already a member." };
    // "no account with that email has signed in yet" — the api's sentence is the
    // honest one, since there is no invitation to send.
    return { error: r.message || "Could not add them." };
  }
  revalidatePath(`/${slug}/settings`);
  return { ok: true };
}

export async function setRole(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const email = String(formData.get("email") ?? "");
  const role = formData.get("role") === "admin" ? "admin" : "member";
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const r = await api.setTeamRole(token, slug, email, role);
  if (!r.ok) return { error: r.message || "Could not change the role." };
  revalidatePath(`/${slug}/settings`);
  return null;
}

export async function removeMember(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const email = String(formData.get("email") ?? "");
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const r = await api.removeTeamMember(token, slug, email);
  if (!r.ok) return { error: r.message || "Could not remove them." };
  // Removing yourself: the page you are on is no longer yours to see.
  if (formData.get("self") === "1") redirect("/");
  revalidatePath(`/${slug}/settings`);
  return null;
}

export async function transferTeam(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const to = String(formData.get("to") ?? "");
  if (!to) return { error: "Pick who takes over." };
  if (String(formData.get("confirm") ?? "") !== slug) return { error: "Type the team handle to confirm." };
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const r = await api.transferTeam(token, slug, to);
  if (!r.ok) return { error: r.message || "Could not transfer the team." };
  revalidatePath(`/${slug}/settings`);
  return { ok: true };
}

export async function destroyTeam(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  if (String(formData.get("confirm") ?? "") !== slug) return { error: "Type the team handle to confirm." };
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const r = await api.deleteTeam(token, slug);
  // 409 carries "still owns N repositories; delete or move them first" — shown as is.
  if (!r.ok) return { error: r.message || "Could not delete the team." };
  revalidatePath("/", "layout");
  redirect("/");
}
