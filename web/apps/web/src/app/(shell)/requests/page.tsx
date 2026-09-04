import type { Metadata } from "next";
import { requireToken } from "@/lib/session";
import { listOrSignIn } from "@/lib/require-api";
import { listRequests, listRegions } from "@/lib/api";
import { kindLabel, summarize } from "@/lib/requests";
import { NewRequestDialog } from "@/components/app/new-request-dialog";
import { when } from "@/lib/time";

export const metadata: Metadata = { title: "My requests" };

export default async function Page() {
  const { session, token } = await requireToken("/requests");
  const [requests, regions] = await Promise.all([
    listRequests(undefined, token),
    listRegions(token),
  ]);
  const rows = listOrSignIn(requests);
  const regionList = listOrSignIn(regions);

  return (
    <div className="mx-auto max-w-3xl px-4 py-8">
      <div className="mb-6 flex items-center justify-between">
        <h1 className="text-lg font-medium">My requests</h1>
        <NewRequestDialog owner={session.user.owner} regions={regionList} />
      </div>

      {rows.length === 0 ? (
        <p className="text-sm2 text-muted-foreground">
          Nothing to show yet — ask for quota, team access, a region, or anything else here.
        </p>
      ) : (
        <ul className="divide-y divide-border border-y border-border">
          {rows.map((r) => (
            <li key={r.id} className="grid gap-1 py-3">
              <div className="flex items-center justify-between">
                <span className="text-sm2 font-medium">{kindLabel(r.kind)}</span>
                <span className="text-caption capitalize text-muted-foreground">{r.state}</span>
              </div>
              <div className="text-sm2 text-muted-foreground">{summarize(r)}</div>
              <div className="text-caption text-muted-foreground">{r.reason}</div>
              {r.state !== "pending" && (r.note || r.resolution) && (
                <div className="text-caption text-foreground">{r.resolution || r.note}</div>
              )}
              {r.createdAt && <div className="text-caption text-muted-foreground">{when(Date.parse(r.createdAt))}</div>}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
