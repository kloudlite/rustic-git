/** The Snapshots surface's two pure pieces: which volumes it lists, and the sentences the
 *  delete dialogs say.
 *
 *  Both live here rather than in the components because both are decided ONCE and read from two
 *  places each — a workspace list and an environment list — and because the delete copy is a
 *  promise about what survives, which is worth a test rather than a re-typed string.
 *
 *  Vocabulary (`crates/workspaces`): a PUSH takes a SNAPSHOT, kept until explicitly deleted. It
 *  is the only thing keeping a volume once its workspace or environment is gone. Sync points are
 *  internal and never listed here. There is no commit and no pin. */
import type { ApiVolumeSummary } from "@/lib/api";

/** A volume whose workspace or environment is gone, but whose snapshots are not. */
export type ArchivedRow = {
  id: string;
  name: string;
  snapshots: number;
  /** RFC3339 of the newest push; `null` while the only push is still being taken. */
  lastPushAt: string | null;
  /** No push ever recorded what it was called; the row shows the id and says so. */
  named: boolean;
};

/** The rows the Snapshots section shows, newest push first.
 *
 *  `deleted` is the api's own answer — no live workspace/environment names this volume — so the
 *  caller never has to diff two listings. A row with no snapshots left is dropped: nothing is
 *  keeping it, and it disappears from the api's listing on its own soon after. */
export function archivedRows(volumes: ApiVolumeSummary[]): ArchivedRow[] {
  return volumes
    .filter((v) => v.deleted && v.snapshots > 0)
    .map((v) => ({
      id: v.name,
      name: v.display_name,
      snapshots: v.snapshots,
      lastPushAt: v.last_push_at,
      named: v.display_name !== v.name,
    }))
    .sort((a, b) => (b.lastPushAt ?? "").localeCompare(a.lastPushAt ?? ""));
}

const s = (n: number) => (n === 1 ? "" : "s");

/** The one line a Delete-volume dialog says. Counted, because "this cannot be undone" only means
 *  something next to how much there is to lose. */
export function deleteVolumeCopy(snapshots: number): string {
  return `Deletes ${snapshots} snapshot${s(snapshots)}. This cannot be undone.`;
}

/** The line a LIVE workspace/environment's Delete dialog carries: what a delete keeps, and what
 *  it does not. The count-free form is for the dialogs that have no listing at hand — vaguer, but
 *  never wrong, which a guessed number would be. */
export function keptSnapshotsCopy(snapshots?: number | null): string {
  if (snapshots == null) {
    return "Your snapshots stay under Snapshots; unpushed changes are deleted.";
  }
  const verb = snapshots === 1 ? "stays" : "stay";
  return `Your ${snapshots} snapshot${s(snapshots)} ${verb} under Snapshots; unpushed changes are deleted.`;
}
