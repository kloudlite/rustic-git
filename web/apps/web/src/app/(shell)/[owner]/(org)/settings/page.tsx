import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { getTeam } from "@/lib/api";
import { TeamSettings } from "@/components/app/team-settings";

export const metadata: Metadata = { title: "Team settings" };

/** Membership is not checked here — see `(org)/page.tsx`: the api answers 404 for a team
 *  the caller is not in, so asking it IS the check. A personal namespace has no team
 *  document and gets the same 404, which is right: a person's settings are at /settings. */
export default async function SettingsPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");
  const token = await apiToken();
  if (!token) redirect("/login");

  const team = await getTeam(token, owner);
  if (!team.ok) {
    if (team.kind === "unauthorized") redirect("/login?from=expired");
    if (team.kind === "notFound") notFound();
    throw new Error(team.message);
  }
  // No <main> here: the (org) layout draws the page container, and a second one indented this
  // page 24px right and 32px down of every sibling.
  return <TeamSettings team={team.value} me={session.user.email} />;
}
