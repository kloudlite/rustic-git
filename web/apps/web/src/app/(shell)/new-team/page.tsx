import type { Metadata } from "next";
import { NewTeamForm } from "@/components/app/new-team-form";
import { requireToken } from "@/lib/session";
import { listOrSignIn } from "@/lib/require-api";
import { listRegions } from "@/lib/api";

export const metadata: Metadata = { title: "New team" };

export default async function NewTeamPage() {
  const { token } = await requireToken("/new-team");
  // Only an active region can take a team; a retired one stays listed for its old records.
  const regions = listOrSignIn(await listRegions(token)).filter((r) => r.status === "active").map((r) => r.id);

  return (
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <NewTeamForm regions={regions} />
      </main>
  );
}
