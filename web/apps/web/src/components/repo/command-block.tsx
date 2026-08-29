"use client";

import { cn } from "@/lib/utils";
import { CopyButton } from "@/components/repo/copy-button";

/** A block of shell you are meant to run, with the one affordance that matters:
 *  taking it. Copy takes the whole block — a person setting up a remote wants all
 *  the lines, and copying them one at a time is the failure this replaces. */
export function CommandBlock({ command, label }: { command: string; label?: string }) {
  return (
    <div className="group relative border border-border bg-card">
      {label && (
        <div className="border-b border-border bg-muted/40 px-4 py-2 text-caption font-medium text-muted-foreground">
          {label}
        </div>
      )}
      <pre className="overflow-x-auto px-4 py-3.5 pr-12 font-mono text-caption leading-relaxed text-foreground/90">
        {command}
      </pre>
      <CopyButton
        value={command}
        size="md"
        className={cn("absolute top-2 right-2 size-8 border border-transparent hover:border-border", label && "top-11")}
      />
    </div>
  );
}
