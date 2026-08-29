"use client";

import { Check, Copy } from "lucide-react";
import { cn } from "@/lib/utils";
import { useCopy } from "@/lib/use-copy";

/** Copy one value. Says it worked, then stops saying it — a tick that never
 *  clears reads as state rather than as feedback. The one copy of this toggle;
 *  `className` places it and `size` picks the icon, which is all six copies differed in. */
export function CopyButton({
  value,
  label = "Copy",
  size = "sm",
  className,
}: {
  value: string;
  label?: string;
  size?: "sm" | "md";
  className?: string;
}) {
  const { copied, copy } = useCopy();
  const icon = size === "sm" ? "size-3.5" : "size-4";
  return (
    <button
      type="button"
      aria-label={copied ? "Copied" : label}
      onClick={() => copy(value)}
      className={cn(
        "flex size-7 items-center justify-center transition-colors",
        copied ? "text-success" : "text-muted-foreground hover:bg-muted hover:text-foreground",
        className,
      )}
    >
      {copied ? <Check className={icon} /> : <Copy className={icon} />}
    </button>
  );
}
