import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import { PageHeader } from "./page-header";

export const metadata: Metadata = { title: "Overview" };

// ponytail: the landing view should surface what needs a decision (pending requests, degraded
// clusters), never a menu — that lands with the task that builds it. This is the header only.
export default async function OverviewPage() {
  await requireSuperadmin("/superadmin");
  return (
    <div>
      <PageHeader title="Overview" purpose="What needs attention across every owner, cluster, and request." />
      <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
        Nothing here yet.
      </p>
    </div>
  );
}
