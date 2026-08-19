import { ChevronDown, GitBranch } from "lucide-react";
import { cn } from "@/lib/utils";

/** The moving part of the Code view: which ref you are looking at. Static until the
 *  API client lands; the shape (branch icon, name, chevron) is the final one. */
export function RefPicker({ current, branches, tags, className }: { current: string; branches: string[]; tags: string[]; className?: string }) {
  return (
    <button
      type="button"
      className={cn("flex h-8 items-center gap-2 border border-edge px-2.5 text-sm2 font-medium transition-colors hover:bg-muted", className)}
      aria-label={`Branch: ${current}. ${branches.length} branches, ${tags.length} tags`}
    >
      <span className="flex min-w-0 items-center gap-2">
        <GitBranch className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="truncate">{current}</span>
      </span>
      <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
    </button>
  );
}
