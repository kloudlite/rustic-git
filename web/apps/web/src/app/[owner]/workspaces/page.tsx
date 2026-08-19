import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { TeamWorkspaces } from "@/components/app/team-workspaces";
import { AppShell } from "@/components/app/app-shell";

export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (owner !== session.user.owner) notFound();
  return (
    <AppShell session={session} active="Workspaces">
      <TeamWorkspaces session={session} />
    </AppShell>
  );
}
