"use client";

import { useState, useTransition } from "react";
import { Loader2 } from "lucide-react";
import { ActivityFeed } from "@/components/app/activity-feed";
import { Button } from "@/components/ui/button";
import { ACTIVITY_MAX, moreActivity } from "@/app/(shell)/[owner]/(org)/activity-actions";
import type { ApiEvent } from "@/lib/api";

function group(events: ApiEvent[]) {
  const now = Date.now() / 1000;
  const day = 24 * 60 * 60;
  const buckets: { label: string; events: ApiEvent[] }[] = [
    { label: "Today", events: [] },
    { label: "Yesterday", events: [] },
    { label: "Earlier", events: [] },
  ];
  for (const e of events) {
    const age = now - e.at;
    buckets[age < day ? 0 : age < 2 * day ? 1 : 2].events.push(e);
  }
  return buckets.filter((b) => b.events.length > 0);
}

/** The home feed, grouped by day, growing in place. There is no activity page
 *  any more: the feed lives here, and "Load more" widens it (`moreActivity`)
 *  rather than sending the reader somewhere else. The button disappears once a
 *  read comes back short or the api's ceiling has been asked for. */
export function RecentActivity({ owner, initial, step }: { owner: string; initial: ApiEvent[]; step: number }) {
  const [events, setEvents] = useState(initial);
  const [limit, setLimit] = useState(step);
  const [done, setDone] = useState(initial.length < step);
  const [pending, start] = useTransition();
  const days = group(events);

  const more = () =>
    start(async () => {
      const next = Math.min(limit + step, ACTIVITY_MAX);
      const got = await moreActivity(owner, next);
      // A short read is the end of the feed; a full one at the ceiling is too.
      setEvents(got.length > events.length ? got : events);
      setLimit(next);
      setDone(got.length < next || next >= ACTIVITY_MAX);
    });

  if (days.length === 0) {
    return (
      <div className="mt-3 border border-border bg-card px-4 py-14 text-center">
        <p className="text-sm2 font-medium">Nothing here yet</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          Push a commit or open a change and it will show up here.
        </p>
      </div>
    );
  }

  return (
    <div className="mt-3 grid gap-8">
      {days.map((d) => (
        <div key={d.label}>
          <h3 className="text-sm2 font-medium text-muted-foreground">{d.label}</h3>
          <ActivityFeed events={d.events} />
        </div>
      ))}
      {!done && (
        <div className="flex justify-center">
          <Button variant="outline" size="sm" onClick={more} disabled={pending}>
            {pending && <Loader2 className="animate-spin" />}Load more
          </Button>
        </div>
      )}
    </div>
  );
}
