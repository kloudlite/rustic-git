"use client";

import { useRouter } from "next/navigation";
import { useEffect, useMemo, useRef, useState } from "react";
import { File, Folder, Search } from "lucide-react";
import { cn } from "@/lib/utils";

export type SearchEntry = { path: string; kind: "dir" | "file" };

/** Subsequence match with a score that prefers runs, word starts and short
 *  paths — enough to make "srau" land on src/auth.rs. Returns null on no match. */
function fuzzy(query: string, path: string): { score: number; hits: number[] } | null {
  const q = query.toLowerCase();
  const p = path.toLowerCase();
  let qi = 0, score = 0, prev = -2;
  const hits: number[] = [];
  for (let i = 0; i < p.length && qi < q.length; i++) {
    if (p[i] !== q[qi]) continue;
    const boundary = i === 0 || "/._-".includes(p[i - 1]);
    score += 10 + (i === prev + 1 ? 15 : 0) + (boundary ? 20 : 0);
    hits.push(i); prev = i; qi++;
  }
  if (qi < q.length) return null;
  const last = p.lastIndexOf("/");
  const inName = hits.filter((h) => h > last).length;
  return { score: score + inName * 8 - p.length * 0.3, hits };
}

function Highlighted({ text, hits }: { text: string; hits: number[] }) {
  const set = new Set(hits);
  return (
    <>
      {text.split("").map((ch, i) => (
        <span key={i} className={cn(set.has(i) && "font-semibold text-foreground")}>{ch}</span>
      ))}
    </>
  );
}

/** "Go to file": type any fragment of a path, in order, and jump. Lives above the
 *  listing so the listing browses and the box jumps — the two ways of getting
 *  somewhere, in the same place. */
export function FileSearch({ base, entries, className }: { base: string; entries: SearchEntry[]; className?: string }) {
  const router = useRouter();
  const [q, setQ] = useState("");
  const [open, setOpen] = useState(false);
  const [cursor, setCursor] = useState(0);
  const box = useRef<HTMLDivElement>(null);

  const results = useMemo(() => {
    if (!q.trim()) return [];
    return entries
      .map((e) => ({ e, m: fuzzy(q.trim(), e.path) }))
      .filter((r): r is { e: SearchEntry; m: NonNullable<ReturnType<typeof fuzzy>> } => r.m !== null)
      .sort((a, b) => b.m.score - a.m.score)
      .slice(0, 12);
  }, [q, entries]);

  useEffect(() => setCursor(0), [q]);
  useEffect(() => {
    const onDoc = (ev: MouseEvent) => { if (!box.current?.contains(ev.target as Node)) setOpen(false); };
    document.addEventListener("mousedown", onDoc);
    return () => document.removeEventListener("mousedown", onDoc);
  }, []);

  const go = (e: SearchEntry) => {
    setOpen(false); setQ("");
    router.push(`${base}/${e.kind === "dir" ? "tree" : "blob"}/${e.path}`);
  };

  return (
    <div ref={box} className={cn("relative", className)}>
      <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
      <input
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
        spellCheck={false}
        className="h-8 w-full border border-edge bg-transparent pr-2.5 pl-8 text-sm2 outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
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
