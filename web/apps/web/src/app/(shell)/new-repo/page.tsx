import type { Metadata } from "next";
import { ownersFor } from "@/lib/owners";
import { NewRepoForm } from "@/components/app/new-repo-form";
import { requireSession } from "@/lib/session";

export const metadata: Metadata = { title: "New repo" };

export default async function NewRepoPage({
  searchParams,
}: {
  searchParams: Promise<{ owner?: string }>;
}) {
  const session = await requireSession("/new-repo");

  const owners = await ownersFor(session);
  const { owner } = await searchParams;
  // Only an owner they actually have: the query is the browser's to set.
  const chosen = owners.find((o) => o.slug === owner)?.slug ?? session.user.owner;

  return (
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <NewRepoForm owners={owners} defaultOwner={chosen} />
      </main>
  );
}
