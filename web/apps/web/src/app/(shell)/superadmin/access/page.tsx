import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import { PageHeader } from "../page-header";

export const metadata: Metadata = { title: "Access" };

export default async function AccessPage() {
  await requireSuperadmin("/superadmin/access");
  return (
    <div>
      <PageHeader title="Access" purpose="Who holds the superadmin claim." />
      <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
        Nothing here yet.
      </p>
    </div>
  );
}
