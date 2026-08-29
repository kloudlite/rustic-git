"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { safeSegment } from "@/lib/slug";
import { sendInvite } from "@/lib/mail";
import { getSession } from "@/lib/session";
import { safeWebsite } from "@/lib/website";

/** Team settings. The api authorizes every one of these on the team's members —
 *  the slug in the form says which team, never whether the caller may touch it.
 *  Nothing here decides membership; a refusal comes back as the api's own words. */

export type TeamState = { ok?: true; error?: string } | null;

/** An invitation's outcome carries the link when the email could NOT be sent, so the inviter
 *  can pass it on themselves. Never when it was sent — a token on screen is a token in a
 *  screenshot. */
export type InviteState = { ok?: true; error?: string; link?: string; notice?: string } | null;

/** The slug goes into a revalidatePath pattern, so a bad one is refused rather
 *  than revalidating something else. A real submission never carries one: the
 *  page fills the field from the route. */
function slugOf(formData: FormData): string | null {
  return safeSegment(String(formData.get("slug") ?? ""));
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

export async function invite(_prev: InviteState, formData: FormData): Promise<InviteState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const email = String(formData.get("email") ?? "").trim();
  const raw = String(formData.get("role") ?? "member");
  const role = raw === "owner" ? "owner" : raw === "admin" ? "admin" : "member";
  if (!email) return { error: "Enter their email." };
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const session = await getSession();

  const r = await api.createInvite(token, slug, email, role);
  if (!r.ok) {
    if (r.kind === "conflict") return { error: "They are already a member." };
    return { error: r.message || "Could not create the invitation." };
  }
  // The link is built from the address the app believes it is on — the same one Auth.js
  // uses for callbacks — so it is right wherever this is deployed.
  const base = (process.env.AUTH_URL ?? "").replace(/\/$/, "");
  const link = `${base}/invite/${r.value.token}`;
  const mail = await sendInvite({
    to: r.value.email,
    teamName: r.value.team_name,
    invitedBy: session?.user.name ?? session?.user.email ?? "A teammate",
    role,
    link,
  });
  revalidatePath(`/${slug}/settings`);
  if (!mail.sent) return { ok: true, link, notice: `${mail.reason} Send them this link instead.` };
  return { ok: true, notice: `Invitation sent to ${r.value.email}.` };
}

export async function revokeInvite(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const id = String(formData.get("id") ?? "");
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const r = await api.revokeInvite(token, slug, id);
  if (!r.ok) return { error: r.message || "Could not withdraw the invitation." };
  revalidatePath(`/${slug}/settings`);
  return null;
}

export async function setRole(_prev: TeamState, formData: FormData): Promise<TeamState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const email = String(formData.get("email") ?? "");
  const raw = String(formData.get("role") ?? "member");
  const role = raw === "owner" ? "owner" : raw === "admin" ? "admin" : "member";
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

export type ProfileState = { ok: true } | { error: string } | null;

/** The public profile and the visibility flag, saved together — they are one form.
 *  `profile` is replace-not-merge on the api, so every field travels every time. */
export async function saveProfile(_prev: ProfileState, formData: FormData): Promise<ProfileState> {
  const slug = slugOf(formData);
  if (!slug) return { error: "That team is not valid." };
  const token = await tokenOr();
  if (typeof token !== "string") return token;
  // The name and description travel too: the api's PATCH replaces both every time.
  const name = String(formData.get("name") ?? "").trim();
  const description = String(formData.get("description") ?? "").trim();
  const profile = {
    public: formData.get("public") === "on",
    tagline: String(formData.get("tagline") ?? "").trim(),
    location: String(formData.get("location") ?? "").trim(),
    website: String(formData.get("website") ?? "").trim(),
    email: String(formData.get("email") ?? "").trim(),
    pins: formData.getAll("pin").map(String),
  };
  // The api refuses this too; checking here turns it into a field error rather than the api's
  // bare 400 text.
  if (profile.website && !safeWebsite(profile.website)) return { error: "Website must start with http:// or https://." };
  const r = await api.updateTeam(token, slug, { name, description, profile });
  if (!r.ok) return { error: r.message || "Could not save." };
  revalidatePath(`/${slug}`, "layout");
  return { ok: true };
}
