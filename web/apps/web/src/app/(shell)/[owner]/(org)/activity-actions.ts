"use server";

import { apiToken } from "@/lib/api-token";
import { activity, type ApiEvent } from "@/lib/api";
import { safeSegment } from "@/lib/slug";

/** The feed's ceiling on the api (`FEED_EVENTS_MAX`): asking for more is clamped
 *  there, so the page stops offering "Load more" once it has asked for this. */
export const ACTIVITY_MAX = 100;

/** The namespace's feed, `limit` deep. There is no cursor on the api — the feed is
 *  a view over a bounded stream, not a table — so "load more" re-reads from the top
 *  with a bigger window and the page swaps the whole list. Cheap at 100 rows. */
export async function moreActivity(owner: string, limit: number): Promise<ApiEvent[]> {
  const o = safeSegment(owner);
  if (!o) return [];
  const token = await apiToken();
  if (!token) return [];
  const r = await activity(token, o, Math.min(limit, ACTIVITY_MAX));
  return r.ok ? r.value : [];
}
