"use client";

import { useEffect } from "react";
import { useRouter } from "next/navigation";

/**
 * Re-fetch the current page's server components on an interval, so state that changes outside the
 * browser — a workspace finishing provisioning, an environment stopping, a merge landing — appears
 * without the user pressing reload.
 *
 * `router.refresh()` rather than a reload: it re-runs the server components and reconciles the
 * result into the existing tree, so open dialogs, form fields and scroll position survive. A real
 * reload would throw away what the user was typing every few seconds.
 *
 * Mounted once in the shell layout, which wraps every signed-in page. A layout stays mounted while
 * the page beneath it changes, so this is one timer for the whole session rather than one per
 * route — and pages that show nothing time-varying cost only the refetch.
 *
 * Paused while the tab is hidden, and refreshed once on becoming visible again: a backgrounded tab
 * that keeps polling is load on the API for something nobody is looking at, and the state a user
 * cares about is the state when they look back.
 *
 * ponytail: polling, not a stream. A watch or SSE would be pushed rather than pulled, but it needs
 * an endpoint, a connection per tab and a reconnect story; this is four lines and correct. Swap it
 * when the refetch cost shows up in the API's own numbers, not before.
 */
export function AutoRefresh({ intervalMs = 10_000 }: { intervalMs?: number }) {
  const router = useRouter();

  useEffect(() => {
    const tick = () => {
      if (document.visibilityState === "visible") router.refresh();
    };
    const id = setInterval(tick, intervalMs);
    // Coming back to the tab should not wait out the rest of the interval.
    document.addEventListener("visibilitychange", tick);
    return () => {
      clearInterval(id);
      document.removeEventListener("visibilitychange", tick);
    };
  }, [router, intervalMs]);

  return null;
}
