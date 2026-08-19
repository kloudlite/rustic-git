import "server-only";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { REPO } from "@/lib/mock-repo";

/** Every repo route: signed in, and the repo exists under this owner. Whether the
 *  visitor may *read* it is the backend's decision per repo; until the API client
 *  lands only the mock repo exists, under its own owner. */
export async function guardRepo(params: Promise<{ owner: string; repo: string }>) {
  const { owner, repo } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (owner !== REPO.owner || repo !== REPO.name) notFound();
  return { session, owner, repo };
}
