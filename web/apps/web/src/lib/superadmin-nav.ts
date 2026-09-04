/** The eight areas of the superadmin rail, in the spec's order — the single source both the
 *  desktop rail and the narrow tab row read, so they can never drift apart. */
export const SUPERADMIN_AREAS: { href: string; label: string }[] = [
  { href: "/superadmin", label: "Overview" },
  { href: "/superadmin/requests", label: "Requests" },
  { href: "/superadmin/owners", label: "Owners" },
  { href: "/superadmin/clusters", label: "Clusters" },
  { href: "/superadmin/monitoring", label: "Monitoring" },
  { href: "/superadmin/audit", label: "Audit" },
  { href: "/superadmin/access", label: "Access" },
  { href: "/superadmin/configuration", label: "Configuration" },
];

/** Longest-prefix match, so `/superadmin/owners/acme` highlights "Owners". `/superadmin` itself
 *  is excluded from the prefix rule — as a prefix of every other href it would otherwise light up
 *  Overview on every page in the area. */
export function activeArea(pathname: string): string {
  const areas = [...SUPERADMIN_AREAS].sort((a, b) => b.href.length - a.href.length);
  for (const a of areas) {
    if (a.href === "/superadmin") {
      if (pathname === a.href) return a.href;
      continue;
    }
    if (pathname === a.href || pathname.startsWith(`${a.href}/`)) return a.href;
  }
  return "/superadmin";
}
