import { notFound } from "next/navigation";
import { loadEnvPage } from "@/lib/env-page";
import { EnvSnapshots } from "@/components/app/env-snapshots";
import { requireToken } from "@/lib/session";

/** One environment's snapshot lineage as a tree, oldest at the top — live or archived, the same records.
 *
 *  Reads by volume id and needs no live Environment: the records live on the server tier and
 *  outlive the thing they were taken of, which is what an archived row IS. */
export default async function Page({ params }: { params: Promise<{ owner: string; id: string }> }) {
  const { owner, id } = await params;
  const { token } = await requireToken(`/${owner}/environments/${id}/snapshots`);

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  const { env, history } = page;

  return (
    <EnvSnapshots
      owner={owner}
      id={id}
      envName={env ? env.name : null}
      // A record carries no author, so the owner it belongs to is what the row can honestly say.
      pusher={env?.owner ?? owner}
      history={history.map((c) => ({ id: c.id, message: c.message, createdAt: c.createdAt, parent: c.parent ?? null }))}
      // The Volume's answer, not the history's: an in-place restore makes an OLDER record the
      // live one. Absent (never restored) means the newest record is current.
      restoredTo={env?.restored_to ?? null}
      restoredAt={env?.restore_requested_at ?? null}
    />
  );
}
