"use client";

import type { SwitcherOwner } from "@/components/app/team-switcher";

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
    <select
      id={id}
      name="owner"
      defaultValue={defaultValue}
      className="h-9 w-full border border-input bg-card px-2.5 font-mono text-sm2 outline-none focus-visible:border-ring"
    >
      {owners.map((o) => (
        <option key={o.slug} value={o.slug}>{o.slug}</option>
      ))}
    </select>
  );
}
