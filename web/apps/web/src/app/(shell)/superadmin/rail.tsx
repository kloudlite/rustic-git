"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { cn } from "@/lib/utils";
import { NavTabs } from "@/components/app/nav-tabs";
import { SUPERADMIN_AREAS, activeArea } from "@/lib/superadmin-nav";

/** The eight-area nav for `/superadmin/*`: a left rail at desktop width, the product's own tab
 *  row below `lg` — same items and the same active area either way, so nothing about the area a
 *  person is in changes shape, only where the row sits. */
export function SuperadminRail() {
  const current = activeArea(usePathname());

  return (
    <>
      <nav
        aria-label="Operations"
        className="hidden shrink-0 lg:block lg:w-52"
      >
        <p className="px-2 pb-2 text-micro font-medium tracking-wide text-muted-foreground">OPERATIONS</p>
        <ul>
          {SUPERADMIN_AREAS.map((a) => {
            const active = a.href === current;
            return (
              <li key={a.href}>
                <Link
                  href={a.href}
                  aria-current={active ? "page" : undefined}
                  className={cn(
                    "block border-l-2 px-3 py-2 text-sm2 transition-colors",
                    active
                      ? "border-primary bg-muted font-medium text-foreground"
                      : "border-transparent text-muted-foreground hover:bg-muted/60 hover:text-foreground",
                  )}
                >
                  {a.label}
                </Link>
              </li>
            );
          })}
        </ul>
      </nav>
      <NavTabs
        tabs={SUPERADMIN_AREAS.map((a) => ({ href: a.href, label: a.label, exact: a.href === "/superadmin" }))}
        activeHref={current}
        className="lg:hidden"
        aria-label="Operations"
      />
    </>
  );
}
