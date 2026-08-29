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
 * Mounted by the pages and layouts whose state changes without the user — the workspace and
 * environment lists, an environment's own pages, a pull request waiting on the worker — and by
 * nothing else. It used to live in the shell layout as one timer for the whole app, which meant a
 * blob page re-highlighted and `/settings` re-listed every credential every 10 s for nobody: with
 * N idle tabs that was the api tier's baseline load.
 *
 * Paused while the tab is hidden, and refreshed once on becoming visible again: a backgrounded tab
 * that keeps polling is load on the API for something nobody is looking at, and the state a user
 * cares about is the state when they look back.
 *
 * A list watching a row finish (a workspace the agent brings up in one to three seconds) mounts a
 * second one at `intervalMs={2_000}` only while such a row exists, so the fast timer lives exactly
 * as long as there is something to catch.
 *
 * ponytail: two timers can coincide and refresh twice in one second; harmless, and cheaper than
 * plumbing a shared scheduler through the layout.
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
