"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Globe, Lock, Package, Search, SquareCode } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Kbd } from "@/components/ui/kbd";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from "@/components/ui/command";
import { sections, settingsSection } from "@/components/app/sections";
import { useOwner } from "@/components/app/shell-nav";
import type { SwitcherOwner } from "@/components/app/team-switcher";
import type { ApiRepo } from "@/lib/api";

/** ⌘K over everything in the current owner. One list, grouped by section, so the
 *  answer to "where is X" is the same keystroke regardless of what X turns out to
 *  be. Filtering is cmdk's — it scores against the item's text, so the item value
 *  carries the words someone would actually type, not just the display label.
 *
 *  Scope: what this owner has. It is a jump-to, not a content search — nothing
 *  here reads file contents, because no endpoint serves them yet. Only repos are
 *  listed: they are the one thing the api serves a list of. */
export function GlobalSearch({
  me,
  owners,
  repos,
}: {
  me: string;
  owners: SwitcherOwner[];
  /** Every repo across every owner; filtered to the owner in the URL here. */
  repos: ApiRepo[];
}) {
  const owner = useOwner(me);
  const [open, setOpen] = useState(false);
  const router = useRouter();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen((v) => !v);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  const go = (href: string) => {
    setOpen(false);
    router.push(href);
  };

  const mine = repos.filter((r) => r.owner === owner);

  return (
    <>
      <Button
        variant="outline"
        onClick={() => setOpen(true)}
        className="hidden w-64 justify-start border-edge font-normal text-muted-foreground hover:border-edge-hover hover:text-foreground md:flex"
      >
        <Search />
        Search
        <Kbd className="ml-auto">⌘K</Kbd>
      </Button>

      <CommandDialog open={open} onOpenChange={setOpen} title="Search" description="Jump to anything in this team">
        <CommandInput placeholder="Search repos…" />
        <CommandList>
          <CommandEmpty>Nothing matches that.</CommandEmpty>

          {mine.length > 0 && (
            <CommandGroup heading="Code Repos">
              {mine.map((r) => (
                <CommandItem key={r._id} value={`repo ${r.name} ${r.description}`} onSelect={() => go(`/${owner}/${r.name}`)}>
                  <SquareCode /> {r.name}
                  <span className="ml-auto flex items-center gap-1 text-caption text-muted-foreground">
                    {r.public ? <Globe className="size-3" /> : <Lock className="size-3" />}
                    {r.public ? "public" : "private"}
                  </span>
                </CommandItem>
              ))}
            </CommandGroup>
          )}

          <CommandSeparator />

          {owners.length > 1 && (
            <CommandGroup heading="Switch to">
              {owners.filter((o) => o.slug !== owner).map((o) => (
                <CommandItem key={o.slug} value={`team ${o.slug} ${o.name}`} onSelect={() => go(`/${o.slug}`)}>
                  <Package /> {o.slug}
                </CommandItem>
              ))}
            </CommandGroup>
          )}

          <CommandGroup heading="Go to">
            {[...sections(owner), settingsSection(owner)].map(({ href, label, icon: Icon }) => (
              <CommandItem key={href} value={`go ${label}`} onSelect={() => go(href)}>
                <Icon /> {label}
              </CommandItem>
            ))}
          </CommandGroup>
        </CommandList>
      </CommandDialog>
    </>
  );
}
