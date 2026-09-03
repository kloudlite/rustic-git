import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { DIMS, dimLabel, type QuotaDim } from "@/lib/quota";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { writeDefault } from "../actions";

export const metadata: Metadata = { title: "Quota defaults" };

/** `default-user`/`default-team` are ordinary owners as far as `GET /v1/quota` is concerned — the
 *  superadmin claim's `may_act_on` arm is what lets this caller read a "owner" they are neither. */
function DefaultForm({ owner, title, limit }: { owner: string; title: string; limit: Record<QuotaDim, number> }) {
  return (
    <form action={writeDefault.bind(null, owner)} className="space-y-3 border border-border bg-card p-4">
      <p className="text-sm2 font-medium">{title}</p>
      <div className="grid grid-cols-2 gap-3">
        {DIMS.map((d) => (
          <label key={d} className="grid gap-1 text-sm2">
            {dimLabel(d)}
            <Input name={d} type="number" min={0} defaultValue={limit[d]} className="h-8" />
          </label>
        ))}
      </div>
      <Button type="submit" size="sm">
        Save
      </Button>
    </form>
  );
}

export default async function Page() {
  const { token } = await requireSuperadmin("/admin/defaults");
  const [user, team] = await Promise.all([
    api.getQuota("default-user", token),
    api.getQuota("default-team", token),
  ]);

  return (
    <div className="grid gap-6 sm:grid-cols-2">
      {user.ok ? (
        <DefaultForm owner="default-user" title="Person default" limit={user.value.limit} />
      ) : (
        <p className="text-sm2 text-destructive">{user.message || "Could not read the person default."}</p>
      )}
      {team.ok ? (
        <DefaultForm owner="default-team" title="Team default" limit={team.value.limit} />
      ) : (
        <p className="text-sm2 text-destructive">{team.message || "Could not read the team default."}</p>
      )}
    </div>
  );
}
