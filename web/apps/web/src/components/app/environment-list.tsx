"use client";

import Link from "next/link";
import { useMemo, useState } from "react";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { ChevronRight, Layers, Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Notices, WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import { when } from "@/lib/time";
import type { ApiEnvironment } from "@/lib/api";
import { ArchivedSnapshots } from "@/components/app/archived-snapshots";
import type { ArchivedRow } from "@/lib/archived";

/** A live row: the environment's own page is one click away, and every action lives THERE.
 *
 *  The row used to carry five buttons. A list is for finding the thing you meant; the actions
 *  belong where the thing is, which is also the only place that can show what they act on. */
function LiveRow({
  owner,
  e,
  lastPushAt,
  snapshots,
}: {
  owner: string;
  e: ApiEnvironment;
  lastPushAt: string | null;
  /** How many snapshots the environment has. `undefined` means the volume listing had no row for
   *  it — never pushed — and the row then says nothing rather than "0 snapshots", which reads as
   *  a fact about a listing that failed just as much as about one that was empty. */
  snapshots?: number;
}) {
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
            {snapshots ? ` · ${snapshots} ${snapshots === 1 ? "snapshot" : "snapshots"}` : ""}
            {lastPushAt && ` · last push ${when(Date.parse(lastPushAt))}`}
          </span>
          <Notices w={e} />
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
  snapshots = {},
}: {
  owner: string;
  environments: ApiEnvironment[];
  archived?: ArchivedRow[];
  /** Volume id → RFC3339 of its newest PUSH, for the live rows' meta line. */
  latest?: Record<string, string | null>;
  /** Volume id → its snapshot count, off the same listing as `latest`. */
  snapshots?: Record<string, number>;
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
                <LiveRow
                  key={e.id}
                  owner={owner}
                  e={e}
                  lastPushAt={latest[e.id] ?? null}
                  snapshots={snapshots[e.id]}
                />
              ))}
            </ul>
          )}

          <ArchivedSnapshots owner={owner} kind="environment" rows={shownArchived} />
        </>
      )}
    </>
  );
}
