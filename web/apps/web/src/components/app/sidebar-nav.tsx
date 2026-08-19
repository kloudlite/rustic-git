import Link from "next/link";
import { cn } from "@/lib/utils";
import { SECTIONS } from "@/components/app/sections";

/** The section list. Rendered once in the sidebar and once in the mobile drawer. */
export function SidebarNav({ active }: { active?: string }) {
  return (
    <nav className="grid gap-0.5">
      {SECTIONS.map(({ href, label, icon: Icon }) => {
        const isActive = active === label;
        return (
          <Link
            key={href}
            href={href}
            aria-current={isActive ? "page" : undefined}
            className={cn(
              "flex h-8 items-center gap-2.5 px-2.5 text-sm2 transition-colors",
              isActive
                ? "bg-sidebar-accent font-medium text-sidebar-accent-foreground"
                : "text-muted-foreground hover:bg-sidebar-accent/60 hover:text-foreground",
            )}
          >
            <Icon className={cn("size-4 shrink-0", isActive ? "text-primary" : "text-muted-foreground")} />
            {label}
          </Link>
        );
      })}
    </nav>
  );
}
