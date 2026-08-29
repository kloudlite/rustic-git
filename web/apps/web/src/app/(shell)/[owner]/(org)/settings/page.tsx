import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { getTeam, listRepos } from "@/lib/api";
import { TeamSettings } from "@/components/app/team-settings";
import { requireToken } from "@/lib/session";

export const metadata: Metadata = { title: "Team settings" };

/** Membership is not checked here — see `(org)/page.tsx`: the api answers 404 for a team
 *  the caller is not in, so asking it IS the check. A personal namespace has no team
 *  document and gets the same 404, which is right: a person's settings are at /settings. */
export default async function SettingsPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const { session, token } = await requireToken(`/${owner}/settings`);

  const [team, repos] = await Promise.all([getTeam(token, owner), listRepos(token, owner)]);
  if (!team.ok) {
    if (team.kind === "unauthorized") redirect("/login?from=expired");
    if (team.kind === "notFound") notFound();
    throw new Error(team.message);
  }
  // No <main> here: the (org) layout draws the page container, and a second one indented this
  // page 24px right and 32px down of every sibling.
  // A failed repo list is not an error page — it only means nothing to pin.
  return <TeamSettings team={team.value} me={session.user.email} repos={repos.ok ? repos.value : []} />;
}
