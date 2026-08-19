"use client";

import { useMemo, useState } from "react";
import { Check, ChevronDown, GitBranch, Search, Tag } from "lucide-react";
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";

/** Which ref you are looking at. A menu with two lists — branches and tags — and a
 *  filter that narrows both. Choosing one changes the ref in the URL once the API
 *  client lands; until then it changes what the picker shows. */
export function RefPicker({
  current,
  branches,
  tags,
  defaultBranch,
  className,
}: {
  current: string;
  branches: string[];
  tags: string[];
  defaultBranch?: string;
  className?: string;
}) {
  const [selected, setSelected] = useState(current);
  const [kind, setKind] = useState<"branches" | "tags">(tags.includes(current) ? "tags" : "branches");
  const [q, setQ] = useState("");
  const [open, setOpen] = useState(false);

  const list = useMemo(() => {
    const src = kind === "branches" ? branches : tags;
    const needle = q.trim().toLowerCase();
    return needle ? src.filter((r) => r.toLowerCase().includes(needle)) : src;
  }, [kind, q, branches, tags]);

  const isTag = tags.includes(selected);

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger
        aria-label={`Switch branch or tag. Current: ${selected}`}
        className={cn(
          "flex h-8 items-center gap-2 border border-edge px-2.5 text-sm2 font-medium transition-colors hover:bg-muted data-open:bg-muted",
          className,
        )}
      >
        <span className="flex min-w-0 items-center gap-2">
          {isTag ? <Tag className="size-3.5 shrink-0 text-muted-foreground" /> : <GitBranch className="size-3.5 shrink-0 text-muted-foreground" />}
          <span className="truncate">{selected}</span>
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
              onKeyDown={(e) => e.stopPropagation()}
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

        <ul role="listbox" aria-label={kind} className="max-h-72 overflow-y-auto py-1">
          {list.length === 0 && (
            <li className="px-3 py-3 text-sm2 text-muted-foreground">No {kind} match &ldquo;{q}&rdquo;</li>
          )}
          {list.map((r) => {
            const active = r === selected;
            return (
              <li key={r} role="option" aria-selected={active}>
                <button
                  type="button"
                  onClick={() => { setSelected(r); setOpen(false); setQ(""); }}
                  className={cn(
                    "flex w-full items-center gap-2.5 px-3 py-1.5 text-left text-sm2 transition-colors hover:bg-muted",
                    active && "font-medium",
                  )}
                >
                  <Check className={cn("size-3.5 shrink-0", active ? "text-foreground" : "text-transparent")} />
                  <span className="min-w-0 flex-1 truncate font-mono text-caption">{r}</span>
                  {r === defaultBranch && (
                    <Badge variant="outline">default</Badge>
                  )}
                </button>
              </li>
            );
          })}
        </ul>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
