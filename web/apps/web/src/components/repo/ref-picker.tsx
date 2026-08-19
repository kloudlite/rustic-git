import { ChevronDown, GitBranch } from "lucide-react";

/** The moving part of the Code view: which ref you are looking at. Static until the
 *  API client lands; the shape (branch icon, name, chevron) is the final one. */
export function RefPicker({ current, branches, tags }: { current: string; branches: string[]; tags: string[] }) {
  return (
    <button
      type="button"
      className="flex h-8 items-center gap-2 border border-edge px-2.5 text-sm2 font-medium transition-colors hover:bg-muted"
      aria-label={`Branch: ${current}. ${branches.length} branches, ${tags.length} tags`}
    >
      <GitBranch className="size-3.5 text-muted-foreground" />
      {current}
      <ChevronDown className="size-3.5 text-muted-foreground" />
    </button>
  );
}
