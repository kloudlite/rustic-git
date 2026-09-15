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
