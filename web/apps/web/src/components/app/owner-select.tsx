"use client";

import type { SwitcherOwner } from "@/components/app/team-switcher";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

/** Which namespace a credential acts in.
 *
 *  Not a nicety: the git fleet compares a credential's owner to the repo's owner
 *  with no membership lookup, so a credential works in one namespace and no other.
 *  Making that a visible choice is the only honest way to present it — a hidden
 *  default would silently produce a token that cannot reach the repo it was made
 *  for. Absent when there is only one namespace to pick. */
export function OwnerSelect({
  owners,
  id,
  defaultValue,
}: {
  owners: SwitcherOwner[];
  id: string;
  defaultValue: string;
}) {
  if (owners.length < 2) return <input type="hidden" name="owner" value={defaultValue} />;
  return (
    <Select name="owner" defaultValue={defaultValue}>
      <SelectTrigger id={id} className="h-9 w-full font-mono text-sm2">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {owners.map((o) => (
          <SelectItem key={o.slug} value={o.slug} className="font-mono">{o.slug}</SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
