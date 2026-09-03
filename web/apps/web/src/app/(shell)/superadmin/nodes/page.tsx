import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";

export const metadata: Metadata = { title: "Nodes" };

export default async function Page() {
  const { token } = await requireSuperadmin("/superadmin/nodes");
  const r = await api.adminListNodes(token);
  const nodes = r.ok ? r.value : [];

  return (
    <ul className="divide-y divide-border border border-border bg-card">
      {nodes.length === 0 ? (
        <li className="px-4 py-8 text-center text-sm2 text-muted-foreground">No nodes reported.</li>
      ) : (
        nodes.map((n) => (
          <li key={n.name} className="flex items-center justify-between gap-3 px-4 py-3 text-sm2">
            <span className="font-medium">{n.name}</span>
            <span className={n.ready ? "text-muted-foreground" : "text-destructive"}>
              {n.ready ? "Ready" : "Not ready"}
            </span>
            {/* The annotation an operator watches a drain through — absent means the node has never
                been asked to leave. */}
            <span className="text-caption text-muted-foreground">
              {n.decommission ? (n.decommissionStatus ?? "decommissioning") : ""}
            </span>
          </li>
        ))
      )}
    </ul>
  );
}
