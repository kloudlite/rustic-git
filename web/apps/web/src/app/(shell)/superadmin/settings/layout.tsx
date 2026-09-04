import type { ReactNode } from "react";
import { NavTabs } from "@/components/app/nav-tabs";

const TABS = [
  { href: "/superadmin/settings", label: "Central", exact: true },
  { href: "/superadmin/settings/clusters", label: "Clusters" },
  { href: "/superadmin/settings/workloads", label: "Workloads" },
];

/** The three tabs spec §5/§6 ask for — Central (one document), Clusters (one panel per region,
 *  `clusters/page.tsx`'s own region picker), and Workloads (spec §6's read-only infra view plus
 *  the manual roll button). */
export default function SettingsLayout({ children }: { children: ReactNode }) {
  return (
    <div className="space-y-6">
      <NavTabs tabs={TABS} aria-label="Settings scope" />
      {children}
    </div>
  );
}
