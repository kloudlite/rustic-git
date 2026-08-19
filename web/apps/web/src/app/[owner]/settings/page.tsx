import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { TeamSettings } from "@/components/app/team-settings";

export const metadata: Metadata = { title: "Team settings" };

export default async function SettingsPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (owner !== session.user.owner) notFound();
  return <TeamSettings session={session} />;
}
