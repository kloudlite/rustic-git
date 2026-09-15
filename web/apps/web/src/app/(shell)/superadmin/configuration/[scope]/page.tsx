import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { historyOf } from "@/lib/settings";
import { PageHeader } from "../../page-header";
import { SettingsEditor } from "./editor";

export async function generateMetadata({ params }: { params: Promise<{ scope: string }> }): Promise<Metadata> {
  const { scope } = await params;
  return { title: `Settings · ${scope}` };
}

/** One scope's editor: `central` or a region id — the same encoding the save and revert actions take. */
export default async function SettingsScopePage({ params }: { params: Promise<{ scope: string }> }) {
  const { scope } = await params;
  const { token } = await requireSuperadmin(`/superadmin/configuration/${scope}`);
  const central = scope === "central";
  const [schemaRes, docRes] = await Promise.all([
    api.adminSettingsSchema(token),
    central ? api.adminCentralSettings(token) : api.adminClusterSettings(scope, token),
  ]);
  if (!docRes.ok && docRes.kind === "notFound") notFound();
  if (!schemaRes.ok) throw new Error(schemaRes.message);
  if (!docRes.ok) throw new Error(docRes.message);

  const doc = docRes.value as { spec?: Record<string, unknown> } & Record<string, unknown>;
  return (
    <div className="space-y-4">
      <Link href="/superadmin/configuration" className="text-sm2 text-muted-foreground hover:text-foreground">
        ← Configuration
      </Link>
      <PageHeader
        title={central ? "Central settings" : `Settings · ${scope}`}
        purpose={
          central
            ? "Read by the server, api, worker and gateway. Every save needs a note and is recorded."
            : "Read by the agent on every node in this region. Every save needs a note and is recorded."
        }
      />
      <SettingsEditor
        scope={scope}
        rows={central ? schemaRes.value.central : schemaRes.value.cluster}
        stored={central ? doc : (doc.spec ?? {})}
        history={historyOf(doc as Parameters<typeof historyOf>[0])}
      />
    </div>
  );
}
