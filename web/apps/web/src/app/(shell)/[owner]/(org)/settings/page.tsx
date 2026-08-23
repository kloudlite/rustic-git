import type { Metadata } from "next";
import { requireSession } from "@/lib/session";
import { NotYet } from "@/components/app/not-yet";

export const metadata: Metadata = { title: "Team settings" };

/** Membership is not checked here — see `(org)/page.tsx`: the api decides who may
 *  act in a namespace, and there is nothing on this page to ask it about yet. */
export default async function SettingsPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  await requireSession();
  return (
    <NotYet title="Team settings">
      Renaming {owner}, inviting members and deleting the team are not available yet.
    </NotYet>
  );
}
