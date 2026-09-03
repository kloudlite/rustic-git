import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { loadEnvPage } from "@/lib/env-page";
import { EnvSettings } from "@/components/app/env-settings";
import { requireToken } from "@/lib/session";

export const metadata: Metadata = { title: "Environment settings" };

export default async function Page({ params }: { params: Promise<{ owner: string; id: string }> }) {
  const { owner, id } = await params;
  const { token } = await requireToken(`/${owner}/environments/${id}/settings`);

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  return (
    <EnvSettings
      owner={owner}
      id={id}
      name={page.name}
      archived={!page.env}
      /* The history this page already holds, not the api's `snapshots` count the list page reads:
         the two are the same number — `snapshot_rows` returns exactly the pushes `snapshots`
         counts, transients excluded — and one round trip is already spent on the history. */
      snapshots={page.history.length}
    />
  );
}
