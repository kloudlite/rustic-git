"use client";

import { useState } from "react";
import Link from "next/link";
import type { AttentionItem } from "@/lib/api";
import { attentionTone } from "@/lib/history";
import { cn } from "@/lib/utils";
import { Pill } from "./ui/pill";
import { EmptyState } from "./ui/data-table";
import { filterAttention, type AttentionFilter } from "./overview";

const TABS: { id: AttentionFilter; label: string }[] = [
  { id: "all", label: "All" },
  { id: "critical", label: "Critical" },
  { id: "warn", label: "Warning" },
];

/** The needs-attention feed. Client only for the one piece of state the mockup's tab row needs —
 *  the rows themselves come from the server render, so the 10 s poll replaces the list underneath
 *  a chosen tab without resetting it. */
export function AttentionFeed({ items, fleetLine }: { items: AttentionItem[]; fleetLine: string }) {
  const [filter, setFilter] = useState<AttentionFilter>("all");
  const shown = filterAttention(items, filter);

  if (items.length === 0) {
    return <EmptyState action={<p className="text-caption tabular-nums text-muted-foreground">{fleetLine}</p>}>Nothing needs a superadmin right now.</EmptyState>;
  }

  return (
    <>
      <div className="flex items-center gap-1 border-b border-border px-4 py-2">
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            onClick={() => setFilter(t.id)}
            aria-pressed={filter === t.id}
            className={cn(
              "h-6 border px-2 text-micro font-medium",
              filter === t.id ? "border-border bg-muted text-foreground" : "border-transparent text-muted-foreground hover:text-foreground",
            )}
          >
            {t.label}
          </button>
        ))}
      </div>
      {shown.length === 0 ? (
        <EmptyState>Nothing in this tab; the other tabs still have rows.</EmptyState>
      ) : (
        <ul>
          {/* Keyed by position: `AttentionItem` has no id, and two rows of the same kind can carry
              the same detail (two nodes draining for one reason), so kind+detail is not unique. */}
          {shown.map((a, i) => (
            <li
              key={`${a.kind}-${i}`}
              className="group/row flex items-center gap-3 border-b border-border px-4 py-2 last:border-0 hover:bg-muted"
            >
              <Pill tone={attentionTone(a.kind)}>{a.kind}</Pill>
              <p className="min-w-0 flex-1 truncate text-sm2">{a.detail}</p>
              <Link href={a.href} className="shrink-0 text-caption text-muted-foreground group-hover/row:text-primary">
                Open
              </Link>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
