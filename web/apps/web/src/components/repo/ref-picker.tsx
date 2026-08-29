"use client";

import { useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { Check, ChevronDown, GitBranch, Search, Tag } from "lucide-react";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";

/** Which ref you are looking at. A menu with two lists — branches and tags — and a
 *  filter that narrows both.
 *
 *  Choosing one navigates: the ref lives in the URL, so a chosen branch survives a
 *  reload, can be linked, and is what every fetch on the page resolves against.
 *  The default branch drops the parameter rather than spelling it out, so the
 *  ordinary case has a clean address.
 *
 *  The trigger reads `current` straight from the URL rather than from state of its
 *  own: back/forward and a `?ref=<sha>` link re-render this same instance with a new
 *  prop, and a copy taken at mount kept showing the old ref. */
export function RefPicker({
  current,
  branches,
  tags,
  defaultBranch,
  base,
  className,
}: {
  current: string;
  branches: string[];
  tags: string[];
  defaultBranch?: string;
  /** Where choosing a ref navigates to — the repo root, or this path within it. */
  base: string;
  className?: string;
}) {
  const router = useRouter();
  const [kind, setKind] = useState<"branches" | "tags">(tags.includes(current) ? "tags" : "branches");
  const [q, setQ] = useState("");

  const list = useMemo(() => {
    const src = kind === "branches" ? branches : tags;
    const needle = q.trim().toLowerCase();
    return needle ? src.filter((r) => r.toLowerCase().includes(needle)) : src;
  }, [kind, q, branches, tags]);

  const isTag = tags.includes(current);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label={`Switch branch or tag. Current: ${current}`}
        className={cn(
          "flex h-8 items-center gap-2 border border-edge px-2.5 text-sm2 font-medium transition-colors hover:bg-muted data-open:bg-muted",
          className,
        )}
      >
        <span className="flex min-w-0 items-center gap-2">
          {isTag ? <Tag className="size-3.5 shrink-0 text-muted-foreground" /> : <GitBranch className="size-3.5 shrink-0 text-muted-foreground" />}
          <span className="truncate">{current}</span>
        </span>
        <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
      </DropdownMenuTrigger>

      <DropdownMenuContent align="start" className="w-80 p-0" onCloseAutoFocus={(e) => e.preventDefault()}>
        <div className="border-b border-border p-2">
          <div className="relative">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              autoFocus
              value={q}
              onChange={(e) => setQ(e.target.value)}
              onKeyDown={(e) => {
                // Typing must reach the input, not the menu's typeahead; the one key that
                // belongs to the menu is the arrow that hands focus down to the first ref.
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  e.currentTarget.closest("[role=menu]")?.querySelector<HTMLElement>("[role=menuitem]")?.focus();
                } else {
                  e.stopPropagation();
                }
              }}
              placeholder={kind === "branches" ? "Find a branch" : "Find a tag"}
              aria-label={kind === "branches" ? "Find a branch" : "Find a tag"} className="h-8 border-edge pl-8 text-sm2"
            />
          </div>
          <Tabs value={kind} onValueChange={(v) => setKind(v as "branches" | "tags")} className="mt-2">
            <TabsList className="w-full">
              <TabsTrigger value="branches" className="flex-1">Branches <span className="text-muted-foreground">{branches.length}</span></TabsTrigger>
              <TabsTrigger value="tags" className="flex-1">Tags <span className="text-muted-foreground">{tags.length}</span></TabsTrigger>
            </TabsList>
          </Tabs>
        </div>

        {/* Menu items, so arrows, Home/End, Enter and Escape come from Radix rather than
            from a listbox of buttons that answered none of them. */}
        <div className="max-h-72 overflow-y-auto py-1">
          {list.length === 0 && (
            <p className="px-3 py-3 text-sm2 text-muted-foreground">No {kind} match &ldquo;{q}&rdquo;</p>
          )}
          {list.map((r) => {
            const active = r === current;
            return (
              <DropdownMenuItem
                key={r}
                aria-current={active ? "true" : undefined}
                onSelect={() => {
                  setQ("");
                  router.push(r === defaultBranch ? base : `${base}?ref=${encodeURIComponent(r)}`);
                }}
                className={cn("gap-2.5 px-3 py-1.5", active && "font-medium")}
              >
                <Check className={cn("size-3.5 shrink-0", active ? "text-foreground" : "text-transparent")} />
                <span className="min-w-0 flex-1 truncate font-mono text-caption">{r}</span>
                {r === defaultBranch && (
                  <Badge variant="outline">default</Badge>
                )}
              </DropdownMenuItem>
            );
          })}
        </div>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
