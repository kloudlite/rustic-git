import { cache } from "react";
import { notFound } from "next/navigation";
import { compareBranches, getPull } from "@/lib/api";
import { parseDiff } from "@/lib/diff";

/**
 * Everything the three PR tabs share.
 *
 * `cache` so the header's counts and the tab's own body come from ONE read: the
 * three tabs are three routes, and without this each would compare the branches
 * again to draw the same numbers.
 *
 * The description and the conversation come from the directory; the commits and
 * the diff are read from git RIGHT NOW, against the two branches the change
 * names. That is why a push updates a PR without anything having to write to it
 * -- and why a merged PR still shows what it contained.
 */
export const pullData = cache(
  async (token: string, owner: string, repo: string, number: number) => {
    const pull = await getPull(token, owner, repo, number);
    if (!pull.ok) {
      if (pull.kind === "notFound") notFound();
      throw new Error(pull.message);
    }
    const pr = pull.value;

    const cmp = await compareBranches(token, owner, repo, pr.base, pr.head);
    const comparison = cmp.ok ? cmp.value : null;
    const diff = comparison ? parseDiff(comparison.diff) : null;

    return {
      pull: pr,
      comparison,
      diff,
      counts: {
        comments: (pr.comments ?? []).length,
        commits: comparison ? comparison.commits.length : null,
        files: diff ? diff.files.length : null,
      },
    };
  },
);
