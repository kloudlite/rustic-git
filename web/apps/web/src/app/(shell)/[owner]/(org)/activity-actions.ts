"use server";

import { apiToken } from "@/lib/api-token";
import { activity, type ApiEvent } from "@/lib/api";
import { safeSegment } from "@/lib/slug";

/** The namespace's feed, `limit` deep. There is no cursor on the api — the feed is
 *  a view over a bounded stream, not a table — so "load more" re-reads from the top
 *  with a bigger window and the page swaps the whole list. Cheap at 100 rows. */
export async function moreActivity(owner: string, limit: number): Promise<ApiEvent[]> {
  const o = safeSegment(owner);
  if (!o) return [];
  const token = await apiToken();
  if (!token) return [];
  const r = await activity(token, o, // The api's own ceiling (`FEED_EVENTS_MAX`); a "use server" file may export only
  // async functions, so the number lives here and in `RecentActivity`, not shared.
  Math.min(limit, 100));
  return r.ok ? r.value : [];
}
