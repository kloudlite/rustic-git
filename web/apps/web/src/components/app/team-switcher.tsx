"use client";

import Link from "next/link";
import { Check, ChevronsUpDown, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

export type SwitcherOwner = { slug: string; name: string; personal?: true };

/** Which namespace you are working in. The person's own handle and every team
 *  they belong to are the same kind of thing here — an owner — so they sit in one
 *  list; only the heading says which is which. Switching is a navigation, not a
 *  setting, so each row is a link and the URL stays the source of truth. */
export function TeamSwitcher({ current, owners }: { current: string; owners: SwitcherOwner[] }) {
  const personal = owners.filter((o) => o.personal);
  const teams = owners.filter((o) => !o.personal);

  const row = (o: SwitcherOwner) => (
    <DropdownMenuItem key={o.slug} asChild>
      <Link href={`/${o.slug}`}>
        <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
        <span className="truncate">{o.slug}</span>
        {o.slug === current && <Check className="ml-auto size-4" />}
      </Link>
    </DropdownMenuItem>
  );

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" className="px-2">
          <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
          {current}
          <ChevronsUpDown className="text-muted-foreground" />
        </Button>
      </DropdownMenuTrigger>

      <DropdownMenuContent align="start" className="w-60">
        <DropdownMenuLabel className="text-caption font-normal text-muted-foreground">
          Personal
        </DropdownMenuLabel>
        {personal.map(row)}

        {teams.length > 0 && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-caption font-normal text-muted-foreground">
              Teams
            </DropdownMenuLabel>
            {teams.map(row)}
          </>
        )}

        <DropdownMenuSeparator />
        <DropdownMenuItem asChild>
          <Link href="/new-team"><Plus className="size-4" /> New team</Link>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
