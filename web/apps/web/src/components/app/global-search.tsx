"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Layers, Package, Search, SquareCode, SquareTerminal, Zap } from "lucide-react";
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
import type { SwitcherOwner } from "@/components/app/team-switcher";
import { REPOS, TEAM_ENVIRONMENTS, TRIGGERS, WORKSPACE_SESSIONS } from "@/lib/mock";

/** ⌘K over everything in the current owner. One list, grouped by section, so the
 *  answer to "where is X" is the same keystroke regardless of what X turns out to
 *  be. Filtering is cmdk's — it scores against the item's text, so the item value
 *  carries the words someone would actually type, not just the display label.
 *
 *  Scope: what this owner has. It is a jump-to, not a content search — nothing
 *  here reads file contents, because no endpoint serves them yet. */
export function GlobalSearch({ owner, owners }: { owner: string; owners: SwitcherOwner[] }) {
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
        <CommandInput placeholder="Search repos, workspaces, environments…" />
        <CommandList>
          <CommandEmpty>Nothing matches that.</CommandEmpty>

          <CommandGroup heading="Code Repos">
            {REPOS.map((r) => (
              <CommandItem key={r.name} value={`repo ${r.name} ${r.description}`} onSelect={() => go(`/${owner}/${r.name}`)}>
                <SquareCode /> {r.name}
                <span className="ml-auto text-caption text-muted-foreground">{r.visibility}</span>
              </CommandItem>
            ))}
          </CommandGroup>

          <CommandGroup heading="Workspaces">
            {WORKSPACE_SESSIONS.map((w) => (
              <CommandItem key={w.id} value={`workspace ${w.id} ${w.definition} ${w.repo} ${w.task ?? ""}`} onSelect={() => go(`/${owner}/workspaces`)}>
                <SquareTerminal /> {w.task ?? `${w.definition} · ${w.repo}`}
                <span className="ml-auto text-caption text-muted-foreground">{w.status}</span>
              </CommandItem>
            ))}
          </CommandGroup>

          <CommandGroup heading="Environments">
            {TEAM_ENVIRONMENTS.map((e) => (
              <CommandItem key={e.name} value={`environment ${e.name}`} onSelect={() => go(`/${owner}/environments`)}>
                <Layers /> {e.name}
              </CommandItem>
            ))}
          </CommandGroup>

          <CommandGroup heading="CI Triggers">
            {TRIGGERS.map((t) => (
              <CommandItem key={t.name} value={`trigger ci ${t.name} ${t.on}`} onSelect={() => go(`/${owner}/ci`)}>
                <Zap /> {t.name}
                <span className="ml-auto text-caption text-muted-foreground">{t.on}</span>
              </CommandItem>
            ))}
          </CommandGroup>

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
