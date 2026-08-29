"use client";

import Link from "next/link";
import { useMemo, useState } from "react";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { ChevronRight, Layers, Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import { when } from "@/lib/time";
import type { ApiEnvironment } from "@/lib/api";

/** An environment that no longer exists, but whose snapshots do — a volume on the server tier with
 *  history and no live `Environment`. Restoring one is the whole reason the row is here. */
export type ArchivedEnv = {
  id: string;
  name: string;
  latest_ms: number | null;
  snapshots: number;
  /** No push ever recorded what it was called; the row shows the id and says so. */
  named: boolean;
};

/** A live row: the environment's own page is one click away, and every action lives THERE.
 *
 *  The row used to carry five buttons. A list is for finding the thing you meant; the actions
 *  belong where the thing is, which is also the only place that can show what they act on. */
function LiveRow({ owner, e, latestMs }: { owner: string; e: ApiEnvironment; latestMs: number | null }) {
  return (
    <li>
      <Link
        href={`/${owner}/environments/${encodeURIComponent(e.id)}`}
        className="flex items-center gap-3.5 px-5 py-3.5 transition-colors hover:bg-muted/40"
      >
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-2.5">
            <span className="truncate text-body font-medium">{e.name}</span>
            <WsEnvStateBadge state={e.state} />
          </span>
          <span className="mt-1 block text-sm2 text-muted-foreground">
            {/* Aggregate view mixes personal and team envs — name the owner when it isn't the page's. */}
            {e.owner !== owner ? `${e.owner} · ` : ""}
            {e.services.length} {e.services.length === 1 ? "service" : "services"} · {e.region}
            {latestMs != null && ` · snapshot ${when(latestMs)}`}
          </span>
        </span>
        <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
      </Link>
    </li>
  );
}

export function EnvironmentList({
  owner,
  environments,
  archived = [],
  latest = {},
}: {
  owner: string;
  environments: ApiEnvironment[];
  archived?: ArchivedEnv[];
  /** Volume id → epoch millis of its newest snapshot, for the live rows' meta line. */
  latest?: Record<string, number | null>;
}) {
  const [q, setQ] = useState("");
  // A row on its way up lands in one to three seconds; the shell's 10 s poll would show it late.
  const busy = environments.some((x) => x.state === "creating");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return environments;
    return environments.filter((e) => e.name.toLowerCase().includes(needle));
  }, [environments, q]);

  const shownArchived = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return archived;
    return archived.filter((a) => a.name.toLowerCase().includes(needle) || a.id.includes(needle));
  }, [archived, q]);

  if (environments.length === 0 && archived.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Layers className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">No environments yet</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          An environment runs one or more services, each backed by a volume.
        </p>
      </div>
    );
  }

  return (
    <>
      {busy && <AutoRefresh intervalMs={2_000} />}
      <div className="relative w-full max-w-xs">
        <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
        <Input
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Filter environments"
          aria-label="Filter environments"
          className="h-8 pl-8 text-sm2"
        />
      </div>

      {shown.length === 0 && shownArchived.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <>
          {shown.length > 0 && (
            <ul className="mt-5 divide-y divide-border border border-border bg-card">
              {shown.map((e) => (
                <LiveRow key={e.id} owner={owner} e={e} latestMs={latest[e.id] ?? null} />
              ))}
            </ul>
          )}

          {/* Archived rows are environments that exist only as DATA. Collapsed, because they are
              history rather than working set — and a native `<details>`, so the disclosure works
              before hydration and needs no state of its own. */}
          {shownArchived.length > 0 && (
            <details className="mt-7 group">
              <summary className="flex cursor-pointer list-none items-center gap-2 text-caption font-semibold tracking-wider text-muted-foreground uppercase">
                <ChevronRight className="size-3.5 transition-transform group-open:rotate-90" aria-hidden />
                Archived ({shownArchived.length})
                <span className="text-caption font-normal tracking-normal normal-case">
                  — environments that are gone; their snapshots are not
                </span>
              </summary>
              <ul className="mt-2.5 divide-y divide-border border border-border bg-card">
                {shownArchived.map((a) => (
                  <li key={a.id}>
                    <Link
                      href={`/${owner}/environments/${encodeURIComponent(a.id)}/snapshots`}
                      className="flex items-center gap-3.5 px-5 py-3.5 transition-colors hover:bg-muted/40"
                    >
                      <span className="min-w-0 flex-1">
                        <span className="flex items-center gap-2.5">
                          <span className="truncate text-body font-medium">{a.name}</span>
                          <span className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground">
                            archived
                          </span>
                        </span>
                        <span className="mt-1 block text-sm2 text-muted-foreground">
                          {a.snapshots} {a.snapshots === 1 ? "snapshot" : "snapshots"}
                          {a.latest_ms != null && ` · last ${when(a.latest_ms)}`}
                          {!a.named && " · name not recorded"}
                        </span>
                      </span>
                      <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
                    </Link>
                  </li>
                ))}
              </ul>
            </details>
          )}
        </>
      )}
    </>
  );
}
