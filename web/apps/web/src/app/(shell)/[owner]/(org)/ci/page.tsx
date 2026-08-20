import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { TeamTriggers } from "@/components/app/team-triggers";

export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (owner !== session.user.owner) notFound();
  return <TeamTriggers session={session} />;
}
