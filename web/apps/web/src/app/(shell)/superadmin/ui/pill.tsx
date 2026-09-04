import { cn } from "@/lib/utils";
import type { Tone } from "@/lib/console";

const TONES: Record<Tone, string> = {
  ok: "border-border text-muted-foreground",
  warn: "border-warning/40 bg-warning/10 text-warning",
  critical: "border-destructive/40 bg-destructive/10 text-destructive",
  info: "border-primary/40 bg-primary/10 text-primary",
  neutral: "border-border bg-muted text-muted-foreground",
};

/** The one status chip of the place. `Badge` covers the rest of the app, but the console needs a
 *  tone axis (`capacityTone` / `attentionTone`) rather than a variant name, and mapping one to the
 *  other at every call site is how a pill ends up amber in one table and grey in the next. */
export function Pill({
  tone = "neutral",
  children,
  className,
}: {
  tone?: Tone;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex h-5 w-fit items-center border px-1.5 text-micro font-medium whitespace-nowrap",
        TONES[tone],
        className,
      )}
    >
      {children}
    </span>
  );
}
