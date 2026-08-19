import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { TeamWorkspaces } from "@/components/app/team-workspaces";

export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (owner !== session.user.owner) notFound();
  return <TeamWorkspaces session={session} />;
}
