import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { createRegionAction } from "../actions";

export const metadata: Metadata = { title: "Regions" };

export default async function Page() {
  // Still `/v1/regions`, unmoved — only the WRITE lives on the admin host now.
  const { token } = await requireSuperadmin("/superadmin/regions");
  const r = await api.listRegions(token);
  const regions = r.ok ? r.value : [];

  return (
    <div className="space-y-6">
      <ul className="divide-y divide-border border border-border bg-card">
        {regions.length === 0 ? (
          <li className="px-4 py-8 text-center text-sm2 text-muted-foreground">No regions yet.</li>
        ) : (
          regions.map((rg) => (
            <li key={rg.id} className="flex items-center justify-between px-4 py-3 text-sm2">
              <span className="font-medium">{rg.id}</span>
              <span className="text-muted-foreground">{rg.status}</span>
            </li>
          ))
        )}
      </ul>

      <form action={createRegionAction} className="flex items-end gap-3 border border-border bg-card p-4">
        <label className="grid gap-1 text-sm2">
          Id
          <Input name="id" required className="h-8" />
        </label>
        <label className="grid gap-1 text-sm2">
          Name
          <Input name="name" required className="h-8" />
        </label>
        <Button type="submit" size="sm">
          Add region
        </Button>
      </form>
    </div>
  );
}
