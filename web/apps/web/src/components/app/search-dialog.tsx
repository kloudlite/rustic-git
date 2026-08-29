"use client";

import { useEffect, useState } from "react";
import { Globe, Lock, Package, Settings, SquareCode } from "lucide-react";
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
import type { SwitcherOwner } from "@/components/app/team-switcher";

type PaletteRepo = { owner: string; name: string; public: boolean; description: string };

/** The ⌘K dialog body, loaded only once ⌘K is first opened — its chunk (cmdk +
 *  radix) and its repo fetch both cost nothing until then. */
export function SearchDialog({
  owner,
  owners,
  open,
  onOpenChange,
  go,
}: {
  owner: string;
  owners: SwitcherOwner[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
  go: (href: string) => void;
}) {
  const [repos, setRepos] = useState<PaletteRepo[] | null>(null);

  useEffect(() => {
    if (!open) return;
    let stale = false;
    fetch(`/api/repos?owner=${encodeURIComponent(owner)}`)
      .then((r) => (r.ok ? r.json() : []))
      .then((v) => {
        // A proxy's HTML error page parses as nothing useful; only a list is a list.
        if (!stale) setRepos(Array.isArray(v) ? v : []);
      })
      .catch(() => {
        if (!stale) setRepos([]);
      });
    return () => {
      stale = true;
    };
  }, [open, owner]);

  const mine = repos ?? [];

  return (
    <CommandDialog open={open} onOpenChange={onOpenChange} title="Search" description="Jump to a repository or section">
      <CommandInput placeholder="Search repos…" />
      <CommandList>
        <CommandEmpty>Nothing matches that.</CommandEmpty>

        {mine.length > 0 && (
          <CommandGroup heading="Code Repos">
            {mine.map((r) => (
              <CommandItem key={r.name} value={`repo ${r.name} ${r.description}`.trim()} onSelect={() => go(`/${owner}/${r.name}`)}>
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
          {/* A person's own namespace has no team settings — the nav hides that tab for it, and
              this list must not offer a page the nav does not. Profile settings are always here:
              the avatar menu is the only other way to them. */}
          {[
            ...sections(owner),
            ...(owners.some((o) => o.personal && o.slug === owner) ? [] : [settingsSection(owner)]),
            { href: "/settings", label: "Profile settings", icon: Settings },
          ].map(({ href, label, icon: Icon }) => (
            <CommandItem key={href} value={`go ${label}`} onSelect={() => go(href)}>
              <Icon /> {label}
            </CommandItem>
          ))}
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
