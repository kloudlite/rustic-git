import "server-only";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { listRepos, type ApiRepo } from "@/lib/api";
import type { Session } from "@/lib/session";

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
export async function guardRepo(
  params: Promise<{ owner: string; repo: string }>,
): Promise<RepoContext> {
  const { owner, repo } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const list = await listRepos(token, owner);
  if (!list.ok) {
    if (list.kind === "notFound" || list.kind === "unauthorized") notFound();
    throw new Error(list.message);
  }
  const meta = list.value.find((r) => r.name === repo);
  if (!meta) notFound();

  return { session, owner, repo, meta, token };
}
