import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { ownersFor } from "@/lib/owners";
import { AppShell } from "@/components/app/app-shell";
import { NewRepoForm } from "@/components/app/new-repo-form";

export const metadata: Metadata = { title: "New repo" };

export default async function NewRepoPage({
  searchParams,
}: {
  searchParams: Promise<{ owner?: string }>;
}) {
  const session = await getSession();
  if (!session) redirect("/login");
  // A repo lives under a namespace, and they have none until they pick a handle.
  if (!session.user.username) redirect("/welcome");

  const owners = await ownersFor(session);
  const { owner } = await searchParams;
  // Only an owner they actually have: the query is the browser's to set.
  const chosen = owners.find((o) => o.slug === owner)?.slug ?? session.user.owner;

  return (
    <AppShell session={session}>
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <NewRepoForm owners={owners} defaultOwner={chosen} />
      </main>
    </AppShell>
  );
}
