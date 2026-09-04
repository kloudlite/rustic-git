import type { ReactNode } from "react";
import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";

export const metadata: Metadata = { title: "Admin" };

/** `/superadmin`, gated on the `superadmin` claim — 404 for anyone without it. The tab row lives
 *  in the shell now (`shell-nav.tsx`'s `SUPERADMIN_TABS`), as the primary row rather than a second
 *  one under the org chrome, since superadmin is not an org. */
export default async function AdminLayout({ children }: { children: ReactNode }) {
  await requireSuperadmin("/superadmin");
  return <main className="mx-auto max-w-page px-6 pt-8 pb-16">{children}</main>;
}
