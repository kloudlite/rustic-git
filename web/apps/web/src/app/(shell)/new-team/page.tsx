import type { Metadata } from "next";
import { NewTeamForm } from "@/components/app/new-team-form";
import { requireSession } from "@/lib/session";

export const metadata: Metadata = { title: "New team" };

export default async function NewTeamPage() {
  await requireSession("/new-team");

  return (
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <NewTeamForm />
      </main>
  );
}
