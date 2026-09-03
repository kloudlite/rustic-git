import type { ReactNode } from "react";
import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import { NavTabs } from "@/components/app/nav-tabs";

export const metadata: Metadata = { title: "Admin" };

const TABS = [
  { href: "/superadmin", label: "Queue", exact: true },
  { href: "/superadmin/usage", label: "Usage" },
  { href: "/superadmin/defaults", label: "Defaults" },
  { href: "/superadmin/regions", label: "Regions" },
  { href: "/superadmin/nodes", label: "Nodes" },
];

/** `/superadmin`, gated on the `superadmin` claim — 404 for anyone without it, same shape as
 *  `[owner]/(org)/layout.tsx`'s tab row, and rooted rather than owner-scoped: nothing under here
 *  belongs to any one owner, it acts ON owners named in each page's own rows. */
export default async function AdminLayout({ children }: { children: ReactNode }) {
  await requireSuperadmin("/superadmin");
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <NavTabs tabs={TABS} aria-label="Admin" />
      <div className="mt-6">{children}</div>
    </main>
  );
}
