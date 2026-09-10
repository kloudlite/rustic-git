import "server-only";
import { fetchRetrying } from "../fetch-retry";
import { fixtureFor } from "@/lib/fixtures/superadmin";
import { log, reason } from "@/lib/log";
import { count } from "@/lib/metrics";

/**
 * The api server, from the web app's server side only: the two bases (user and admin), the one
 * call shape every module below uses, and the result type a page renders from.
 */
export const logger = log("web::lib::api");

/**
 * The api server, from the web app's server side only.
 *
 * Nothing here reaches the browser. The web app holds no database connection and
 * no signing key — it holds a peer secret used exactly once per session, at
 * sign-in, and after that presents the user's own token. Data lives behind the
 * api server so there is one writer, one place that decides what a valid handle
 * is, and one process holding the credentials.
 */

export const BASE = (process.env.KLOUDLITE_API_URL ?? "http://kloudlite-api").replace(/\/$/, "");
// A second base, because the admin surface is a SEPARATE process on a separate host (design doc
// §5) — pointing this at the same host as `BASE` would be a silent way to lose the whole point of
// the split, so there is no fallback to `KLOUDLITE_API_URL` here.
export const ADMIN_BASE = (process.env.KLOUDLITE_ADMIN_API_URL ?? "http://kloudlite-admin").replace(/\/$/, "");
export const PEER_SECRET = process.env.KLOUDLITE_PEER_SECRET ?? "";
/** How long a call may take before it is answered `unavailable` instead. */
export const TIMEOUT_MS = 5_000;
/** For the calls that run git upstream — a compare or a commit is not a row read. */
export const SLOW_TIMEOUT_MS = 15_000;

export type ApiUser = {
  _id: string;
  name: string;
  username?: string;
};

export type ApiTeam = {
  _id: string;
  name: string;
  createdBy: string;
  members: { user: string; role: "owner" | "admin" | "member" }[];
};

export type SignIn = { user: ApiUser; token: string | null; expiresIn: number };

/** What a call can end in. `conflict` is not a failure — a taken handle is an
 *  ordinary answer the form has to render, so it is a value rather than a throw. */
export type ApiResult<T> =
  | { ok: true; value: T }
  | { ok: false; kind: "conflict" | "invalid" | "unauthorized" | "forbidden" | "notFound" | "unavailable"; message: string };

export async function callAgainst<T>(
  base: string,
  path: string,
  init: RequestInit & { token?: string; asUser?: string },
): Promise<ApiResult<T>> {
  // `KLOUDLITE_ADMIN_FIXTURES=1` answers READS from a seeded module instead of the network, so
  // every superadmin screen renders with realistic data on a laptop with no cluster (spec §C: the
  // screens are verified by screenshot before merge). It sits in `callAgainst` rather than in
  // `adminCall` because three of the console's reads — the superadmin list, the regions list and
  // the default quotas — go to the ordinary host, and a guard that covered only the admin one
  // would leave three sections blank. Unseeded paths answer `undefined` and fall through, so the
  // rest of the app is untouched; the flag is unset in every deployment.
  //
  // Only GET is faked: a decision, a roll or a drain must still reach the real api, because a
  // write that "succeeds" against nothing is a screenshot that lies.
  if (process.env.KLOUDLITE_ADMIN_FIXTURES === "1" && (init.method ?? "GET") === "GET") {
    const seeded = fixtureFor(path);
    if (seeded !== undefined) return { ok: true, value: seeded as T };
  }

  // Which of the two processes this call is against; the label the counter is read by.
  const upstream = base === ADMIN_BASE ? "admin" : "api";

  const headers = new Headers(init.headers);
  headers.set("content-type", "application/json");
  if (init.token) {
    headers.set("authorization", `Bearer ${init.token}`);
  } else if (init.asUser) {
    // Only for sign-in, which is where a token comes from. Every later call
    // carries the user's own token instead.
    headers.set("x-kloudlite-peer", PEER_SECRET);
    headers.set("x-kloudlite-owner", init.asUser);
  }

  let res: Response;
  try {
    // Bounded: a hung api pod must not pin a render, or every refresh stacks another one until
    // the heap is gone. A timeout is the same answer as an unreachable server. Callers that do
    // real work upstream (a compare, a commit) pass a longer `signal`.
    res = await fetchRetrying(`${base}${path}`, {
      signal: AbortSignal.timeout(TIMEOUT_MS),
      ...init,
      headers,
      cache: "no-store",
    });
  } catch (e) {
    // The api server being unreachable is not the user's problem to read about — but it is
    // exactly what an operator is looking for, so it goes to the log and the counter here
    // rather than dying inside the sentence the page renders.
    count("upstream_requests_total", { upstream, status: "error" });
    logger.error("api.upstream.failed", { upstream, path, method: init.method ?? "GET", error: reason(e) });
    return { ok: false, kind: "unavailable", message: "The service is unavailable. Try again." };
  }
  count("upstream_requests_total", { upstream, status: String(res.status) });
  // 4xx are ordinary answers (a taken handle, a role that is not enough); only the upstream
  // failing at its own end is an event.
  if (res.status >= 500) {
    logger.warn("api.upstream.failed", { upstream, path, method: init.method ?? "GET", status: res.status });
  }

  if (res.ok) {
    // 204 carries no body, and `json()` on an empty one throws. A delete answers
    // with nothing to say, which is not the same as failing.
    if (res.status === 204) return { ok: true, value: undefined as T };
    return { ok: true, value: (await res.json()) as T };
  }

  const message = (await res.text()).trim();
  if (res.status === 409) return { ok: false, kind: "conflict", message };
  if (res.status === 400) return { ok: false, kind: "invalid", message };
  // 422 means a value the person typed is unusable, and the api's sentence names WHICH one —
  // showing "the service is unavailable" for that would hide the only useful part.
  if (res.status === 422) {
    let named = message;
    try {
      named = (JSON.parse(message) as { error?: string }).error ?? message;
    } catch {
      // Not the JSON envelope; the raw text is still better than a generic sentence.
    }
    return { ok: false, kind: "invalid", message: named };
  }
  if (res.status === 401) return { ok: false, kind: "unauthorized", message };
  // Signed in, a member, and still refused: the role is not enough. The api says
  // which role it wanted, and that sentence is for the person.
  if (res.status === 403) return { ok: false, kind: "forbidden", message };
  // The api answers 404 for a namespace the caller may not act in, deliberately:
  // whether it exists is not theirs to learn. The page renders it as one too.
  if (res.status === 404) return { ok: false, kind: "notFound", message };
  return { ok: false, kind: "unavailable", message: "The service is unavailable. Try again." };
}

export function call<T>(path: string, init: RequestInit & { token?: string; asUser?: string }): Promise<ApiResult<T>> {
  return callAgainst<T>(BASE, path, init);
}

/** Every call the /admin area makes. Same shape as `call`, against the admin host — never the
 *  ordinary one, so an admin page cannot accidentally fall back to a route that does not exist
 *  there (it would 404, not silently authorize as an ordinary user, but the intent is clearer
 *  with its own function). */
export function adminCall<T>(path: string, init: RequestInit & { token?: string; asUser?: string }): Promise<ApiResult<T>> {
  return callAgainst<T>(ADMIN_BASE, path, init);
}
