import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { TeamEnvironments } from "@/components/app/team-environments";

export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (owner !== session.user.owner) notFound();
  return <TeamEnvironments session={session} />;
}
