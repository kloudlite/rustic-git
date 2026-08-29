"use server";

import { redirect } from "next/navigation";
import { tokenOr } from "@/lib/api-token";
import { createRepo } from "@/lib/api";

export type NewRepoState = { error?: string } | null;

export async function create(_prev: NewRepoState, formData: FormData): Promise<NewRepoState> {
  const owner = String(formData.get("owner") ?? "").trim();
  const name = String(formData.get("name") ?? "").trim();
  const description = String(formData.get("description") ?? "").trim();
  const visibility = formData.get("visibility") === "public" ? "public" : "private";

  if (!owner) return { error: "Pick who owns this repo." };
  if (!name) return { error: "Give the repo a name." };
  // Said here as well as by the api so the form can answer without a round trip.
  // The api still decides — this is a courtesy, never the gate.
  if (!/^[A-Za-z0-9._-]+$/.test(name) || name === "." || name === "..") {
    return { error: "Names may use letters, digits, dots, dashes and underscores." };
  }

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await createRepo(token, { owner, name, visibility, description });
  if (!r.ok) {
    if (r.kind === "conflict") return { error: `${owner}/${name} already exists.` };
    return { error: r.message || "Could not create the repo." };
  }
  redirect(`/${owner}/${name}`);
}
