"use client";

import { useRouter } from "next/navigation";
import { useEffect, useMemo, useRef, useState } from "react";
import { File, Folder, Search } from "lucide-react";
import { cn, pathHref } from "@/lib/utils";
import { Input } from "@/components/ui/input";
import { fuzzy } from "@/lib/fuzzy";

export type SearchEntry = { path: string; kind: "dir" | "file" };

/** Consecutive hits render as one span: a 60-char path was 60 elements. */
function Highlighted({ text, hits }: { text: string; hits: number[] }) {
  const set = new Set(hits);
  const runs: { s: string; hit: boolean }[] = [];
  for (let i = 0; i < text.length; i++) {
    const hit = set.has(i);
    const last = runs[runs.length - 1];
    if (last && last.hit === hit) last.s += text[i];
    else runs.push({ s: text[i], hit });
  }
  return (
    <>
      {runs.map((r, i) =>
        r.hit ? <span key={i} className="font-semibold text-foreground">{r.s}</span> : <span key={i}>{r.s}</span>,
      )}
    </>
  );
}

/** "Go to file": type any fragment of a path, in order, and jump. Lives above the
 *  listing so the listing browses and the box jumps — the two ways of getting
 *  somewhere, in the same place. */
export function FileSearch({
  base,
  entries,
  refName,
  className,
}: {
  base: string;
  entries: SearchEntry[];
  /** The ref being browsed. Dropping it sent every jump to the default branch, so
   *  searching from a branch quietly navigated off it. */
  refName?: string;
  className?: string;
}) {
  const router = useRouter();
  const [q, setQ] = useState("");
  const [open, setOpen] = useState(false);
  // Cursor is stored with the query it belongs to, so a new query starts at 0
  // without an effect that sets state after render.
  const [cursorFor, setCursorFor] = useState<{ q: string; i: number }>({ q: "", i: 0 });
  const cursor = cursorFor.q === q ? cursorFor.i : 0;
  const setCursor = (next: number | ((c: number) => number)) =>
    setCursorFor({ q, i: typeof next === "function" ? next(cursor) : next });
  const box = useRef<HTMLDivElement>(null);

  const results = useMemo(() => {
    if (!q.trim()) return [];
    return entries
      .map((e) => ({ e, m: fuzzy(q.trim(), e.path) }))
      .filter((r): r is { e: SearchEntry; m: NonNullable<ReturnType<typeof fuzzy>> } => r.m !== null)
      .sort((a, b) => b.m.score - a.m.score)
      .slice(0, 12);
  }, [q, entries]);

  useEffect(() => {
    const onDoc = (ev: MouseEvent) => { if (!box.current?.contains(ev.target as Node)) setOpen(false); };
    document.addEventListener("mousedown", onDoc);
    return () => document.removeEventListener("mousedown", onDoc);
  }, []);

  const go = (e: SearchEntry) => {
    setOpen(false); setQ("");
    const q = refName ? `?ref=${encodeURIComponent(refName)}` : "";
    router.push(`${base}/${e.kind === "dir" ? "tree" : "blob"}/${pathHref(e.path)}${q}`);
  };

  return (
    <div ref={box} className={cn("relative", className)}>
      <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
      <Input
        type="text"
        value={q}
        onChange={(ev) => { setQ(ev.target.value); setOpen(true); }}
        onFocus={() => setOpen(true)}
        onKeyDown={(ev) => {
          if (ev.key === "ArrowDown") { ev.preventDefault(); setCursor((c) => Math.min(c + 1, results.length - 1)); }
          else if (ev.key === "ArrowUp") { ev.preventDefault(); setCursor((c) => Math.max(c - 1, 0)); }
          else if (ev.key === "Enter" && results[cursor]) { ev.preventDefault(); go(results[cursor].e); }
          else if (ev.key === "Escape") { setOpen(false); (ev.target as HTMLInputElement).blur(); }
        }}
        placeholder="Go to file"
        aria-label="Go to file"
        role="combobox"
        aria-expanded={open && results.length > 0}
        aria-controls="file-search-results"
        autoComplete="off"
        spellCheck={false} className="h-8 border-edge pl-8 text-sm2"
      />
      {open && q.trim() && (
        <ul
          id="file-search-results"
          role="listbox"
          className="absolute top-full right-0 left-0 z-30 mt-1 max-h-80 overflow-y-auto border border-border bg-popover shadow-md"
        >
          {results.length === 0 && (
            <li className="px-3 py-3 text-sm2 text-muted-foreground">Nothing matches &ldquo;{q}&rdquo;</li>
          )}
          {results.map(({ e, m }, i) => (
            <li
              key={e.path}
              role="option"
              aria-selected={i === cursor}
              onMouseEnter={() => setCursor(i)}
              onMouseDown={(ev) => { ev.preventDefault(); go(e); }}
              className={cn(
                "flex cursor-pointer items-center gap-2.5 px-3 py-2 font-mono text-caption text-muted-foreground",
                i === cursor && "bg-muted",
              )}
            >
              {e.kind === "dir" ? <Folder className="size-4 shrink-0 text-primary/70" /> : <File className="size-4 shrink-0" />}
              <span className="truncate"><Highlighted text={e.path} hits={m.hits} /></span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
