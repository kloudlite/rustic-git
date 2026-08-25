"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { Camera, Layers, Search, SquareTerminal } from "lucide-react";
import { Input } from "@/components/ui/input";
import type { ApiVolumeSummary } from "@/lib/api";

/** Read-only, so this is `image-list.tsx`'s shape without the copy-line: filter
 *  locally, link out to the detail page rather than expanding inline — a second
 *  page keeps this list the same height whether a volume has one commit or a
 *  hundred. */
export function VolumeList({ owner, volumes }: { owner: string; volumes: ApiVolumeSummary[] }) {
  const [q, setQ] = useState("");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return volumes;
    return volumes.filter((v) => v.name.toLowerCase().includes(needle));
  }, [volumes, q]);

  if (volumes.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Camera className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">No snapshots yet</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          A workspace or environment gets a volume here once it commits.
        </p>
      </div>
    );
  }

  return (
    <>
      <div className="relative w-full max-w-xs">
        <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
        <Input
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Filter volumes"
          aria-label="Filter volumes"
          className="h-8 pl-8 text-sm2"
        />
      </div>

      {shown.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {shown.map((v) => (
            <li key={v.name}>
              <Link
                href={`/${owner}/snapshots/${encodeURIComponent(v.name)}?kind=${v.kind}`}
                className="flex items-center gap-4 px-5 py-4 transition-colors hover:bg-muted/50"
              >
                {v.kind === "workspace" ? (
                  <SquareTerminal className="size-4 shrink-0 text-muted-foreground" aria-hidden />
                ) : (
                  <Layers className="size-4 shrink-0 text-muted-foreground" aria-hidden />
                )}
                <span className="min-w-0 flex-1">
                  <span className="truncate text-body font-medium">{v.name}</span>
                  <span className="mt-1 block text-sm2 text-muted-foreground capitalize">{v.kind}</span>
                </span>
                {!v.volume && (
                  <span className="shrink-0 text-caption text-muted-foreground">Not pushed yet</span>
                )}
              </Link>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
