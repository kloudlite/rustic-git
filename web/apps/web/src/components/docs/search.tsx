"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { FileText, Hash, Search as SearchIcon } from "lucide-react";
import { CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Kbd } from "@/components/ui/kbd";

type Entry = { title: string; section: string; href: string; headings: { text: string; id: string }[] };

/** ⌘K over every page title and heading. Client-side: the index is a few kilobytes and
 *  arrives with the layout, so a search never waits on a request. */
export function Search({ index }: { index: Entry[] }) {
  const [open, setOpen] = useState(false);
  const router = useRouter();
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);
  const go = (href: string) => {
    setOpen(false);
    router.push(href);
  };
  return (
    <>
      <button type="button" className="docs-search-btn" onClick={() => setOpen(true)} aria-label="Search documentation">
        <SearchIcon className="size-3.5" />
        <span className="docs-search-text">Search docs</span>
        <Kbd className="docs-search-kbd">⌘K</Kbd>
      </button>
      <CommandDialog open={open} onOpenChange={setOpen} title="Search documentation" description="Pages and sections">
        <CommandInput placeholder="Search pages and sections…" />
        <CommandList>
          <CommandEmpty>Nothing matches.</CommandEmpty>
          <CommandGroup heading="Pages">
            {index.map((e) => (
              <CommandItem key={e.href} value={`${e.title} ${e.section}`} onSelect={() => go(e.href)}>
                <FileText className="size-4 text-muted-foreground" />
                <span>{e.title}</span>
                <span className="ml-auto text-xs text-muted-foreground">{e.section}</span>
              </CommandItem>
            ))}
          </CommandGroup>
          <CommandGroup heading="Sections">
            {index.flatMap((e) =>
              e.headings.map((h) => (
                <CommandItem key={`${e.href}#${h.id}`} value={`${h.text} ${e.title}`} onSelect={() => go(`${e.href}#${h.id}`)}>
                  <Hash className="size-4 text-muted-foreground" />
                  <span>{h.text}</span>
                  <span className="ml-auto text-xs text-muted-foreground">{e.title}</span>
                </CommandItem>
              )),
            )}
          </CommandGroup>
        </CommandList>
      </CommandDialog>
    </>
  );
}
