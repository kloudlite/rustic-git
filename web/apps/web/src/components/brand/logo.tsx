import { cn } from "@/lib/utils";

/** The mark. Sharp-cornered, drawn rather than an image, so it inherits currentColor. */
export function Mark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 24 24" fill="none" className={cn("size-6", className)} aria-hidden>
      <rect x="1" y="1" width="22" height="22" className="stroke-current" strokeWidth="2" />
      <path d="M7 12h10M12 7v10" className="stroke-current" strokeWidth="2" strokeLinecap="square" />
    </svg>
  );
}

export function Wordmark({ className }: { className?: string }) {
  return (
    <span className={cn("flex items-center gap-2.5", className)}>
      <span className="flex size-7 items-center justify-center bg-primary text-primary-foreground">
        <Mark className="size-4" />
      </span>
      <span className="text-[15px] font-bold tracking-tight">kloudlite</span>
    </span>
  );
}
