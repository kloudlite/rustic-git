import { ABSOLUTE } from "@/lib/time";

/** The grace a removed member's data waits out — `MEMBER_REMOVAL_GRACE` on the api. */
export const REMOVAL_GRACE_MS = 7 * 86_400_000;

/** What removing someone does, said before it is done. `now` is a parameter so the date is testable. */
export function removalConfirm(name: string, team: string, now: number): string {
  const date = ABSOLUTE.format(now + REMOVAL_GRACE_MS);
  return `Remove ${name} from ${team}? Their bench, workspaces and space choice are deleted on ${date} (7 days). Pushed snapshots, repos, images and team environments stay with the team. Takes effect within about 5 minutes.`;
}

export function pauseConfirm(name: string): string {
  return `Pause ${name}? Their bench and workspaces stop; nothing is deleted; unpausing restores access but starts nothing. Takes effect within about 5 minutes.`;
}

/** Delete-now's own confirm, named after `removalConfirm`: irreversible, so the person and the
 *  team it happens in are both said out loud — load-bearing when the same handle is pending in
 *  two teams at once (the superadmin console's Pending removals lists every team, unlike a team's
 *  own settings page). */
export function deleteNowConfirm(owner: string, team: string): string {
  return `Type ${owner} to delete their bench, workspaces and space choice in ${team} now`;
}

/** The key a pending-removal row is tracked and opened by — `team/owner`, never `owner` alone: a
 *  listing that spans every team (the superadmin console's) can carry the same handle twice, once
 *  per team it was removed from, and keying on the owner would let confirming one row silently
 *  open — or submit against — the other. */
export function removalKey(team: string, owner: string): string {
  return `${team}/${owner}`;
}
