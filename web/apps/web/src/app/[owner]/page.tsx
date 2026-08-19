import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { Dashboard } from "@/components/app/dashboard";

/** The owner's Code Repos. Only the signed-in owner's namespace exists yet; the
 *  browse API decides per repo what a visitor may see, so this page will widen
 *  once it reads from it rather than from mock data. */
export default async function OwnerPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (owner !== session.user.owner) notFound();
  return <Dashboard session={session} />;
}
