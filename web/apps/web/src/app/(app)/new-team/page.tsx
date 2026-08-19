import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { AppShell } from "@/components/app/app-shell";
import { NewTeamForm } from "@/components/app/new-team-form";

export const metadata: Metadata = { title: "New team" };

export default async function NewTeamPage() {
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  return (
    <AppShell session={session}>
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <NewTeamForm />
      </main>
    </AppShell>
  );
}
