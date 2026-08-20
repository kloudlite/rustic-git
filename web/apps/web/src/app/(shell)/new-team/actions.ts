"use server";

import { redirect } from "next/navigation";
import { apiToken } from "@/lib/api-token";
import { createTeam } from "@/lib/api";

export type NewTeamState = { error?: string } | null;

export async function create(_prev: NewTeamState, formData: FormData): Promise<NewTeamState> {
  const name = String(formData.get("name") ?? "").trim();
  const slug = String(formData.get("slug") ?? "").trim().toLowerCase();
  if (!name) return { error: "Give the team a name." };
  if (!slug) return { error: "Pick a handle for the team." };

  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const r = await createTeam(token, slug, name);
  if (!r.ok) {
    // A taken handle is an ordinary answer, not a failure: the form stays up and
    // says so. Handles are shared with usernames, so it may be a person's.
    if (r.kind === "conflict") return { error: `${slug} is taken.` };
    return { error: r.message || "Could not create the team." };
  }
  redirect(`/${r.value._id}`);
}
