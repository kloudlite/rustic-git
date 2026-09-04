import type { ReactNode } from "react";
import { NavTabs } from "@/components/app/nav-tabs";

const TABS = [
  { href: "/superadmin/settings", label: "Central", exact: true },
  { href: "/superadmin/settings/clusters", label: "Clusters" },
];

/** The two tabs spec §5 asks for — Central (one document) and Clusters (one panel per region,
 *  `clusters/page.tsx`'s own region picker). */
export default function SettingsLayout({ children }: { children: ReactNode }) {
  return (
    <div className="space-y-6">
      <NavTabs tabs={TABS} aria-label="Settings scope" />
      {children}
    </div>
  );
}
