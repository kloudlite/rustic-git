import "server-only";
import { cache } from "react";
import { notFound, redirect } from "next/navigation";
import { getRepo, type ApiRepo } from "@/lib/api";
import { requireToken, type Session } from "@/lib/session";

export type RepoContext = {
  session: NonNullable<Session>;
  owner: string;
  repo: string;
  meta: ApiRepo;
  /** The session's api token, for the browse calls each page makes. */
  token: string;
};

/** Every repo route: signed in, and this repo exists in a namespace the caller may
 *  act in. Membership is not decided here — the api answers 404 for a namespace
 *  that is not theirs, so asking it is the check. */
/** Wrapped in `cache`, so the layout and the page it wraps resolve the repo ONCE
 *  per request instead of each making the same call to the api. */
export const guardRepo = cache(async function guardRepo(
  owner: string,
  repo: string,
): Promise<RepoContext> {
  const { session, token } = await requireToken(`/${owner}/${repo}`);

  const one = await getRepo(token, owner, repo);
  if (!one.ok) {
    // `unauthorized` is the api refusing our token, not a missing repo. Treating
    // it as 404 made an expired session look like every repo had been deleted.
    if (one.kind === "unauthorized") redirect("/login?from=expired");
    if (one.kind === "notFound") notFound();
    throw new Error(one.message);
  }
  return { session, owner, repo, meta: one.value, token };
});
