"use client";

import { useMemo, useState } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { OwnerRow, RequestDoc } from "@/lib/api";
import { KINDS, kindLabel } from "@/lib/requests";
import {
  contextLine,
  filterQueue,
  summaryLine,
  type QueueFilter,
} from "@/lib/request-queue";
import { when } from "@/lib/time";
import { Section } from "../ui/section";
import {
  DataTable,
  EmptyState,
  RowActions,
  Td,
  Th,
  Tr,
} from "../ui/data-table";
import { Pill } from "../ui/pill";
import { DecisionPanel } from "./decision-panel";

export function RequestQueue({
  rows,
  usage,
}: {
  rows: RequestDoc[];
  usage: OwnerRow[];
}) {
  const router = useRouter();
  const path = usePathname();
  const sp = useSearchParams();
  const usageByOwner = useMemo(
    () => new Map(usage.map((u) => [u.owner, u])),
    [usage],
  );
  const teams = useMemo(
    () => new Set(usage.filter((u) => u.isTeam).map((u) => u.owner)),
    [usage],
  );
  const [selected, setSelected] = useState<{ id: string; deny: boolean } | null>(null);
  // Read once: the render has to stay pure, and a cutoff ticking under an open page would
  // silently drop rows out of the "older than 1 day" filter while somebody is reading them.
  const [now] = useState(() => Date.now());

  const tab = sp.get("state") === "decided" ? "decided" : "open";
  const filter: QueueFilter = {
    kind: (sp.get("kind") as QueueFilter["kind"] | null) ?? "any",
    ownerType:
      (sp.get("ownerType") as QueueFilter["ownerType"] | null) ?? "any",
    age: (sp.get("age") as QueueFilter["age"] | null) ?? "any",
  };

  function setParam(key: string, value: string) {
    const next = new URLSearchParams(sp.toString());
    if (value === "any" || value === "") next.delete(key);
    else next.set(key, value);
    router.replace(`${path}?${next}`, { scroll: false });
  }

  const base = rows.filter((r) =>
    tab === "open" ? r.state === "pending" : r.state !== "pending",
  );
  const shown = filterQueue(
    base,
    filter,
    now,
    usage.length > 0 ? teams : undefined,
  );
  const active = rows.find((r) => r.id === selected?.id) ?? null;

  return (
    <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1fr)_420px]">
      <Section
        eyebrow="Queue"
        title="Requests"
        count={`${shown.length} ${tab}`}
        bare
        toolbar={
          <Tabs
            value={tab}
            onValueChange={(v) => setParam("state", v === "open" ? "" : v)}
          >
            <TabsList className="h-8">
              <TabsTrigger value="open" className="text-sm2">
                Open
              </TabsTrigger>
              <TabsTrigger value="decided" className="text-sm2">
                Decided
              </TabsTrigger>
            </TabsList>
          </Tabs>
        }
      >
        {/* The filters sit in their own row rather than the section header: three selects plus
            the tabs do not fit beside the title at this column width, and squeezing them there
            truncated the title itself. */}
        <div className="flex flex-wrap items-center gap-2 border-b border-border px-3 py-2">
          <Select
            value={filter.kind}
            onValueChange={(v) => setParam("kind", v)}
          >
            {/* The label is passed as children, not left to `SelectValue` to derive: Radix knows
                the selected item's text only after its items mount, so the server render — and
                any screenshot of it — would show an empty box. */}
            <SelectTrigger className="h-8 w-28 text-sm2" aria-label="Kind">
              <SelectValue>
                {filter.kind === "any" ? "All kinds" : kindLabel(filter.kind)}
              </SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="any">All kinds</SelectItem>
              {KINDS.map((k) => (
                <SelectItem key={k} value={k}>
                  {kindLabel(k)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={filter.ownerType}
            onValueChange={(v) => setParam("ownerType", v)}
          >
            <SelectTrigger
              className="h-8 w-28 text-sm2"
              aria-label="Owner type"
            >
              <SelectValue>
                {
                  { any: "All owners", person: "People", team: "Teams" }[
                    filter.ownerType
                  ]
                }
              </SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="any">All owners</SelectItem>
              <SelectItem value="person">People</SelectItem>
              <SelectItem value="team">Teams</SelectItem>
            </SelectContent>
          </Select>
          <Select value={filter.age} onValueChange={(v) => setParam("age", v)}>
            <SelectTrigger className="h-8 w-28 text-sm2" aria-label="Age">
              <SelectValue>
                {
                  { any: "Any age", "1d": "Over 1 day", "7d": "Over 7 days" }[
                    filter.age
                  ]
                }
              </SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="any">Any age</SelectItem>
              <SelectItem value="1d">Over 1 day</SelectItem>
              <SelectItem value="7d">Over 7 days</SelectItem>
            </SelectContent>
          </Select>
        </div>
        {shown.length === 0 ? (
          <EmptyState>No request matches these filters.</EmptyState>
        ) : (
          // Fixed layout: the summary column ellipses instead of wrapping, so a long reason
          // cannot reflow every other column under the 10 s poll.
          <DataTable className="[&_table]:table-fixed">
            <thead>
              <tr>
                <Th className="w-28">Kind</Th>
                <Th className="w-36">Owner</Th>
                <Th>Request</Th>
                <Th className="w-24">Requester</Th>
                <Th numeric className="w-24">
                  Age
                </Th>
                {/* No Status column on Open: every row on this tab is open by definition. */}
                {tab === "decided" && <Th className="w-24">Status</Th>}
                <Th className="w-20" />
              </tr>
            </thead>
            <tbody>
              {shown.map((r) => (
                <Tr
                  key={r.id}
                  className={selected?.id === r.id ? "bg-muted" : undefined}
                >
                  <Td className="h-14">
                    <Pill tone="info">{r.kind}</Pill>
                  </Td>
                  <Td className="h-14">
                    <span className="flex items-center gap-1.5">
                      <span className="truncate">{r.owner}</span>
                      {teams.has(r.owner) && <Pill tone="neutral">team</Pill>}
                    </span>
                  </Td>
                  <Td className="h-14 max-w-0">
                    <p className="truncate">{summaryLine(r)}</p>
                    <p className="truncate text-caption text-muted-foreground">
                      {contextLine(r, usageByOwner.get(r.owner))}
                    </p>
                  </Td>
                  <Td className="h-14 truncate text-muted-foreground">
                    {r.requestedBy}
                  </Td>
                  <Td numeric className="h-14 whitespace-nowrap">
                    {when(new Date(r.createdAt ?? 0).getTime())}
                  </Td>
                  {tab === "decided" && (
                    <Td className="h-14">
                      <Pill tone={r.state === "approved" ? "ok" : "critical"}>
                        {r.state}
                      </Pill>
                    </Td>
                  )}
                  <Td className="h-14">
                    <RowActions>
                      <button
                        type="button"
                        className="text-caption text-muted-foreground hover:text-primary"
                        onClick={() => setSelected({ id: r.id, deny: false })}
                      >
                        Open
                      </button>
                      {r.state === "pending" && (
                        <button
                          type="button"
                          className="text-caption text-muted-foreground hover:text-destructive"
                          onClick={() => setSelected({ id: r.id, deny: true })}
                        >
                          Deny
                        </button>
                      )}
                    </RowActions>
                  </Td>
                </Tr>
              ))}
            </tbody>
          </DataTable>
        )}
        {shown.length > 0 && (
          <p className="border-t border-border px-3 py-2 text-caption text-muted-foreground">
            Showing {shown.length === base.length ? `all ${shown.length}` : `${shown.length} of ${base.length}`} {tab}{" "}
            {shown.length === 1 ? "request" : "requests"}. One request per owner and kind may be pending at a time.
          </p>
        )}
      </Section>
      <DecisionPanel
        request={active}
        usage={active ? usageByOwner.get(active.owner) : undefined}
        // The owner's own recent decisions, out of the page's one fetch rather than a second read.
        denyIntent={selected?.deny ?? false}
        history={
          active
            ? rows
                .filter(
                  (r) =>
                    r.owner === active.owner &&
                    r.state !== "pending" &&
                    r.id !== active.id,
                )
                .sort(
                  (a, b) =>
                    new Date(b.decidedAt ?? 0).getTime() -
                    new Date(a.decidedAt ?? 0).getTime(),
                )
                .slice(0, 3)
            : []
        }
        onDone={() => {
          setSelected(null);
          router.refresh();
        }}
      />
    </div>
  );
}
