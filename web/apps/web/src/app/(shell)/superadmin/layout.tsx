import type { ReactNode } from "react";
import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import { SuperadminRail } from "./rail";

export const metadata: Metadata = { title: "Admin" };

/** `/superadmin`, gated on the `superadmin` claim — 404 for anyone without it. Its own eight-area
 *  rail lives here rather than in the shell's top tab row: that row is for an org, and superadmin
 *  acts on owners rather than being one. Stacked (tab row above content) below `lg`, two columns
 *  (rail beside content) at `lg` and up. */
export default async function AdminLayout({ children }: { children: ReactNode }) {
  await requireSuperadmin("/superadmin");
  return (
    // Full width, 28 px gutter (spec §C): this place is a console over the whole fleet — a
    // 1120 px column would put the nodes table and the decision panel in a letterbox. The shell's
    // header stretches with it (`shellWidthClass`); every other place keeps the centred container.
    <main className="w-full px-7 pt-8 pb-16">
      <div className="flex flex-col gap-6 lg:flex-row lg:gap-8">
        <SuperadminRail />
        <div className="min-w-0 flex-1">{children}</div>
      </div>
    </main>
  );
}
