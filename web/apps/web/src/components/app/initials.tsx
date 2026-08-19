import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { cn } from "@/lib/utils";

/** A person as a tile of initials, on shadcn's Avatar so it takes an image the day
 *  one exists. `size` in Tailwind units; muted by default, primary for "you". */
export function Initials({
  name,
  size = 6,
  tone = "muted",
  className,
}: {
  name: string;
  size?: 6 | 7 | 8;
  tone?: "muted" | "primary";
  className?: string;
}) {
  const text = name.split(/[\s@._-]+/).filter(Boolean).map((p) => p[0]).slice(0, 2).join("").toUpperCase();
  return (
    <Avatar className={cn(size === 6 ? "size-6" : size === 7 ? "size-7" : "size-8", className)}>
      <AvatarFallback
        className={cn(
          "text-micro font-semibold",
          tone === "primary" ? "bg-primary text-primary-foreground" : "bg-muted text-muted-foreground",
        )}
      >
        {text}
      </AvatarFallback>
    </Avatar>
  );
}
