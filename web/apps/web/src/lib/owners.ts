import "server-only";
import { apiToken } from "@/lib/api-token";
import { listTeams } from "@/lib/api";
import type { Session } from "@/lib/session";

export type Owner = { slug: string; name: string; personal?: true };

/** Every namespace this person can act in: themselves, then their teams.
 *
 *  A team list that fails to load is not worth an error page — the caller gets
 *  just the person, and the next render tries again. That degrades the switcher
 *  and the owner picker; it never grants anything, because the api decides
 *  membership again on every write. */
export async function ownersFor(session: NonNullable<Session>): Promise<Owner[]> {
  const me: Owner = { slug: session.user.owner, name: session.user.name, personal: true };
  const token = await apiToken();
  if (!token) return [me];
  const teams = await listTeams(token);
  if (!teams.ok) return [me];
  return [me, ...teams.value.map((t) => ({ slug: t._id, name: t.name }))];
}
