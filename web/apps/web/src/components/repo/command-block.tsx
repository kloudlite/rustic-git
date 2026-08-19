"use client";

import { useState } from "react";
import { Check, Copy } from "lucide-react";
import { cn } from "@/lib/utils";

/** A block of shell you are meant to run, with the one affordance that matters:
 *  taking it. Copy takes the whole block — a person setting up a remote wants all
 *  the lines, and copying them one at a time is the failure this replaces. */
export function CommandBlock({ command, label }: { command: string; label?: string }) {
  const [copied, setCopied] = useState(false);

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
      <button
        type="button"
        aria-label={copied ? "Copied" : "Copy"}
        onClick={async () => {
          await navigator.clipboard.writeText(command);
          setCopied(true);
          setTimeout(() => setCopied(false), 1600);
        }}
        className={cn(
          "absolute top-2 right-2 flex size-8 items-center justify-center border border-transparent transition-colors",
          label && "top-11",
          copied ? "text-success" : "text-muted-foreground hover:border-border hover:bg-muted hover:text-foreground",
        )}
      >
        {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
      </button>
    </div>
  );
}
