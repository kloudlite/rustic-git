"use client";

import { useState } from "react";
import { cn } from "@/lib/utils";
import type { CloneUrls } from "@/lib/clone";
import { CopyButton } from "@/components/repo/copy-button";

/** The repo's address, with the protocol as a choice rather than two boxes.
 *  Whichever is selected is what the setup commands below use, so the person
 *  copies a consistent pair instead of an ssh remote and https instructions. */
export function RemotePicker({
  urls,
  onChange,
}: {
  urls: CloneUrls;
  onChange?: (kind: "ssh" | "https") => void;
}) {
  const [kind, setKind] = useState<"ssh" | "https">("ssh");
  const value = urls[kind];

  return (
    <div className="flex h-9 items-stretch border border-input bg-card">
      <div className="flex shrink-0 items-center border-r border-input p-0.5">
        {(["ssh", "https"] as const).map((k) => (
          <button
            key={k}
            type="button"
            onClick={() => {
              setKind(k);
              onChange?.(k);
            }}
            className={cn(
              "h-full px-2.5 text-caption font-medium uppercase tracking-label transition-colors",
              kind === k ? "bg-muted text-foreground" : "text-muted-foreground hover:text-foreground",
            )}
          >
            {k}
          </button>
        ))}
      </div>
      <input
        readOnly
        value={value}
        onFocus={(e) => e.currentTarget.select()}
        aria-label={`${kind} remote`}
        className="min-w-0 flex-1 bg-transparent px-3 font-mono text-caption outline-none"
      />
      <CopyButton value={value} label="Copy remote" size="md" className="h-auto w-10 shrink-0 border-l border-input" />
    </div>
  );
}
