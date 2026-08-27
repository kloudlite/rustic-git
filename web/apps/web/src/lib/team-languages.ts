import "server-only";
import { defaultBranch, files, refs } from "@/lib/browse";
import { breakdown, type LanguageShare } from "@/lib/languages";

/**
 * What the pinned repos are written in, read the way a stranger reads them: no
 * token anywhere. `breakdown` weighs by bytes, so concatenating the file lists
 * and asking once is the same answer as merging per-repo shares would be, and
 * one call less.
 *
 * ponytail: first four pins only — the page shows a handful of languages and each
 * pin costs two round trips; widen if pins ever get a bigger budget.
 */
export async function pinnedLanguages(owner: string, pins: string[]): Promise<LanguageShare[]> {
  const lists = await Promise.all(
    pins.slice(0, 4).map(async (repo) => {
      // A pin can be renamed, deleted or made private while it is still pinned;
      // that repo drops out of the tally rather than emptying the whole block.
      try {
        const all = await refs(undefined, owner, repo);
        if (!all.ok) return [];
        const head = defaultBranch(all.value);
        if (!head) return [];
        return await files(undefined, owner, repo, head.oid);
      } catch {
        return [];
      }
    }),
  );
  return breakdown(lists.flat());
}
