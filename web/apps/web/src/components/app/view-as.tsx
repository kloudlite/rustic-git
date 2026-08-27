"use client";

import Link from "next/link";
import { Check, ChevronDown, Eye } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

export type View = "member" | "public";

/** The public view is the same page under a query, not a second route: a member
 *  previewing it stays where they are, and dropping the query is how you come back. */
export function hrefFor(slug: string, view: View) {
  return view === "public" ? `/${slug}?view=public` : `/${slug}`;
}

const LABEL: Record<View, string> = { member: "Member", public: "Public" };

export function ViewAs({ slug, view }: { slug: string; view: View }) {
  const row = (v: View) => (
    <DropdownMenuItem key={v} asChild>
      <Link href={hrefFor(slug, v)}>
        <span className="truncate">{LABEL[v]}</span>
        {v === view && <Check className="ml-auto size-4" />}
      </Link>
    </DropdownMenuItem>
  );

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" className="h-8 border border-input bg-card px-2">
          <Eye className="text-muted-foreground" />
          <span className="text-muted-foreground">View as:</span>
          <b className="font-medium">{LABEL[view]}</b>
          <ChevronDown className="text-muted-foreground" />
        </Button>
      </DropdownMenuTrigger>

      <DropdownMenuContent align="end" className="w-40">
        {row("member")}
        {row("public")}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
