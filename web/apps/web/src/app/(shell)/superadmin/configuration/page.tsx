import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import { PageHeader } from "../page-header";

export const metadata: Metadata = { title: "Configuration" };

export default async function ConfigurationPage() {
  await requireSuperadmin("/superadmin/configuration");
  return (
    <div>
      <PageHeader title="Configuration" purpose="The live cluster and central settings, read-only for now." />
      <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
        Nothing here yet.
      </p>
    </div>
  );
}
