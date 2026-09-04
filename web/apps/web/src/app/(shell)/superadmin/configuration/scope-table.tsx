"use client";

import { useState } from "react";
import { Input } from "@/components/ui/input";
import type { SettingsSchemaRow } from "@/lib/api";
import { effectiveValue, fmt } from "@/lib/settings";
import { matchesSearch, takesEffect } from "@/lib/config-rows";
import { Section } from "../ui/section";
import { DataTable, EmptyState, Td, Th, Tr } from "../ui/data-table";
import { Pill } from "../ui/pill";

export type Scope = {
  /** `Central · cluster/settings`, or `Cluster · {region} · ClusterSettings/default`. */
  title: string;
  /** Who reads this scope and how often — the sentence under the scope's own title. */
  readers: string;
  rows: SettingsSchemaRow[];
  /** The stored document for this scope, keyed by the same names the schema rows use. */
  stored: Record<string, unknown>;
  error?: string | null;
};

/** One section per scope, and ONE search box above them all: a knob is looked up by name without
 *  the operator first knowing which scope defines it, so a per-section filter would be the wrong
 *  shape. Read-only — the page never writes; the tiles above say where a value changes. */
export function ScopeTables({ scopes }: { scopes: Scope[] }) {
  const [q, setQ] = useState("");

  return (
    <div className="space-y-4">
      <Input
        value={q}
        onChange={(e) => setQ(e.target.value)}
        placeholder="Search fields"
        aria-label="Search fields"
        className="h-8 w-64"
      />
      {scopes.map((scope) => {
        const rows = scope.rows.filter((r) => matchesSearch(r, q));
        return (
          <Section
            key={scope.title}
            eyebrow="Scope"
            title={scope.title}
            count={`${rows.length} fields`}
            bare
            toolbar={<span className="text-caption text-muted-foreground">{scope.readers}</span>}
          >
            {scope.error && <p className="px-4 py-2 text-caption text-destructive">{scope.error}</p>}
            {rows.length === 0 ? (
              <EmptyState>No field here matches that. Clear the search to see this scope again.</EmptyState>
            ) : (
              <DataTable>
                <thead>
                  <tr>
                    <Th>Field</Th>
                    <Th numeric>Current value</Th>
                    <Th>Source</Th>
                    <Th numeric>Range</Th>
                    <Th>Takes effect</Th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((row) => {
                    const { value, source } = effectiveValue(scope.stored[row.name], row.env, row.default);
                    return (
                      <Tr key={row.name}>
                        <Td className="font-mono">{row.name}</Td>
                        <Td numeric className="font-medium">{fmt(value)}</Td>
                        <Td><Pill tone={source === "stored" ? "info" : "neutral"}>{source}</Pill></Td>
                        <Td numeric className="text-muted-foreground">
                          {row.range ? `${row.range.min} – ${row.range.max}` : "—"}
                        </Td>
                        <Td className="text-muted-foreground">{takesEffect(row)}</Td>
                      </Tr>
                    );
                  })}
                </tbody>
              </DataTable>
            )}
          </Section>
        );
      })}
    </div>
  );
}
