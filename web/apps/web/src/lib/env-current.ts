import { snapshotTime } from "@/lib/snapshot";

/** The shape both callers' rows already have: the api's `ApiCommitRecord` and the Snapshots
 *  tab's `SnapshotNode`. Generic so each caller gets its OWN row type back. */
export type CurrentInput = { id: string; createdAt: string | null; parent?: string | null };

export type EnvCurrent<T> = {
  /** The record the environment sits on, or `null` — archived, or a `restoredTo` from elsewhere. */
  current: T | null;
  /** A `restoredTo` naming no record here: a restore grafted ANOTHER volume's snapshot in place.
   *  Badging any record `current` would claim the environment is on a snapshot it is not. */
  foreign: string | null;
};

/** Where an environment sits, decided ONCE for both the header and the Snapshots tab.
 *
 *  Never restored: the newest record (one straight chain). Restored: the newest record pushed
 *  AFTER the restore that descends from the restored one — the environment moved on to it — else
 *  the restored record itself. Its older children are the branches the environment left behind.
 *
 *  This lived twice, with two different answers, and the header's was the wrong one: it fell
 *  through to `history[0]` after an in-place restore and after a foreign one.
 *
 *  `history` is newest first, as `/v1/volumes/{name}/history` returns it. */
export function envCurrent<T extends CurrentInput>(
  history: T[],
  { live, restoredTo, restoredAt }: { live: boolean; restoredTo: string | null; restoredAt: string | null },
): EnvCurrent<T> {
  if (!live) return { current: null, foreign: null };
  if (restoredTo === null) return { current: history[0] ?? null, foreign: null };

  const byId = new Map(history.map((h) => [h.id, h]));
  const restored = byId.get(restoredTo) ?? null;
  if (!restored) return { current: null, foreign: restoredTo };

  // `NaN > since` is false, so an undated record never wins this — an unorderable row is the
  // truth (`lib/snapshot.ts`), and guessing would move the badge onto it.
  const since = restoredAt ? Date.parse(restoredAt) : 0;

  // Walk parent links looking for `anc`, but only through records pushed AFTER the restore —
  // stepping onto an older record that isn't `anc` itself means we've wandered onto the branch
  // the environment left (its own pre-restore ancestors), not the one it moved to.
  const descends = (n: T, anc: string): boolean => {
    for (let p: T | undefined = n; p; p = p.parent ? byId.get(p.parent) : undefined) {
      if (p.id === anc) return true;
      if (!(snapshotTime(p) > since)) return false;
    }
    return false;
  };
  return {
    current: history.find((h) => snapshotTime(h) > since && descends(h, restored.id)) ?? restored,
    foreign: null,
  };
}
