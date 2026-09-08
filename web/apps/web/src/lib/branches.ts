/**
 * The two decisions the Branches page makes about a branch, kept out of the component
 * so they are testable without React.
 */
import type { ApiProtection } from "@/lib/api";
import type { Ref } from "@/lib/browse";

/** The branch the fleet refuses to delete. Fixed on the server (`refs::DEFAULT_BRANCH`),
 *  so it is a constant here too rather than a guess from the ref list. */
export const DEFAULT_BRANCH = "main";

export const branchName = (r: Ref) => r.name.replace(/^refs\/heads\//, "");

/** Branches only, default first, then alphabetically. */
export function orderBranches(refs: Ref[]): Ref[] {
  return refs
    .filter((r) => r.kind === "branch" && r.name.startsWith("refs/heads/"))
    .sort((a, b) => {
      const an = branchName(a);
      const bn = branchName(b);
      if (an === bn) return 0;
      if (an === DEFAULT_BRANCH) return -1;
      if (bn === DEFAULT_BRANCH) return 1;
      return an.localeCompare(bn);
    });
}

/** The pattern that forbids deleting `branch`, if one does. Mirrors `Protection::matches`
 *  in `crates/storage/src/refmeta.rs` exactly — a TRAILING `*` is a prefix match, anything
 *  else is the whole name — so the button the page disables and the rule the server
 *  enforces agree. Only `no_delete` matters here; a no-force rule forbids nothing about
 *  deleting. */
export function protectedBy(rules: ApiProtection[], branch: string): string | undefined {
  return rules.find((r) => {
    if (!r.no_delete) return false;
    return r.pattern.endsWith("*")
      ? branch.startsWith(r.pattern.slice(0, -1))
      : r.pattern === branch;
  })?.pattern;
}
