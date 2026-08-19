import Link from "next/link";
import type { LucideIcon } from "lucide-react";
import { cn } from "@/lib/utils";

export type NavTab = {
  href: string;
  label: string;
  icon?: LucideIcon;
  count?: number;
  /** Pushed to the far end of the row (Settings). */
  end?: boolean;
};

/** Underline tabs, used for every level of navigation so they all behave alike.
 *  The label is a chip that lights on hover; the underline belongs to the label,
 *  not the hit area, so it is exactly as wide as the word it marks. Height and
 *  type are fixed here — callers cannot make one row taller than another. */
export function NavTabs({
  tabs,
  active,
  className,
  "aria-label": ariaLabel,
}: {
  tabs: NavTab[];
  active?: string;
  className?: string;
  "aria-label"?: string;
}) {
  return (
    <nav className={cn("-mb-px flex items-stretch", className)} aria-label={ariaLabel}>
      {tabs.map(({ href, label, icon: Icon, count, end }) => {
        const isActive = active === label;
        return (
          <Link
            key={href}
            href={href}
            aria-current={isActive ? "page" : undefined}
            className={cn(
              "group relative flex h-11 items-center px-1 text-sm2 outline-none",
              end && "ml-auto",
              isActive ? "text-foreground" : "text-muted-foreground hover:text-foreground",
            )}
          >
            <span className="flex h-7 items-center gap-2 whitespace-nowrap px-2 transition-colors group-hover:bg-muted group-focus-visible:ring-2 group-focus-visible:ring-ring">
              {Icon && <Icon className={cn("size-4", isActive ? "text-foreground" : "text-muted-foreground")} />}
              <span className={cn(isActive && "font-medium")}>{label}</span>
              {typeof count === "number" && (
                <span
                  className={cn(
                    "min-w-5 px-1.5 text-center text-micro font-medium leading-4",
                    isActive ? "bg-muted text-foreground" : "bg-muted/60 text-muted-foreground",
                  )}
                >
                  {count}
                </span>
              )}
            </span>
            <span
              aria-hidden
              className={cn(
                "absolute inset-x-1 bottom-0 h-0.5 transition-colors",
                isActive ? "bg-primary" : "bg-transparent",
              )}
            />
          </Link>
        );
      })}
    </nav>
  );
}
