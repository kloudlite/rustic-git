import "server-only";
import { files, log } from "@/lib/browse";
import { breakdown, type LanguageShare } from "@/lib/languages";

export type Contributor = { name: string; commits: number };

/**
 * What the About rail says about a repo.
 *
 * One function, used by every page that draws the rail, because the rail is a
 * fact about the REPO — the same in every directory and on every file. Computing
 * it per page is how the Languages block came to appear at the root and vanish
 * one directory down, and how Contributors showed on the tree and not on a file.
 *
 * Both halves are keyed by the commit id, so a second page asking for them is a
 * cache hit rather than a second walk.
 */
/** How many paths the rail asks for. The server would hand back 5000 by default, and every one
 *  of them crossed two hops and landed in the RSC payload for go-to-file on every page view.
 *  ponytail: past this the language bar is a sample and go-to-file is incomplete; a
 *  `/languages/{oid}` answer computed where the objects are, plus server-side file search, is
 *  the upgrade when a repo that size shows up. */
export const RAIL_PATH_CAP = 2000;

export async function repoRail(token: string, owner: string, repo: string, oid: string) {
  const [blobs, recent] = await Promise.all([
    files(token, owner, repo, oid, "", RAIL_PATH_CAP),
    log(token, owner, repo, oid, 50),
  ]);

  const commits = recent.ok ? recent.value : [];
  const byAuthor = new Map<string, number>();
  for (const c of commits) byAuthor.set(c.author, (byAuthor.get(c.author) ?? 0) + 1);

  return {
    blobs,
    commits,
    languages: breakdown(blobs) as LanguageShare[],
    // Recent history, not all of it — the tooltip says so rather than implying a
    // total nobody counted.
    contributors: [...byAuthor.entries()]
      .map(([name, count]) => ({ name, commits: count }))
      .sort((a, b) => b.commits - a.commits)
      .slice(0, 12) as Contributor[],
  };
}
