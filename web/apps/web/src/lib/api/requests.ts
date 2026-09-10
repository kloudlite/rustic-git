import type { QuotaDim } from "@/lib/quota";
import { adminCall, call } from "./client";
import type { QuotaRequestDoc } from "./workspaces";

/**
 * Quota and access requests: the owner's ask and the superadmin's decision.
 */
import type { RequestDoc } from "@/lib/requests";

/** `requestedBy` is set by the api from the session, never sent — a request that could name its
 *  own author is not evidence of who asked. */
export function createRequest(
  body: { owner?: string; kind: string; reason: string } & Record<string, unknown>,
  token: string,
) {
  return call<RequestDoc>("/v1/requests", { method: "POST", token, body: JSON.stringify(body) });
}

/** The caller's own and their teams'; `owner` narrows to one they may act on. */
export function listRequests(owner: string | undefined, token: string) {
  const q = owner ? `?owner=${encodeURIComponent(owner)}` : "";
  return call<RequestDoc[]>(`/v1/requests${q}`, { method: "GET", token });
}

/** The whole queue, every owner and every kind, unioned over the new CRD and the legacy one.
 *  `kind`/`owner`/`state` are server-side narrowing; anything finer stays client-side. */
export function adminListRequests(token: string, filter?: { kind?: string; owner?: string; state?: string }) {
  const q = new URLSearchParams();
  if (filter?.kind) q.set("kind", filter.kind);
  if (filter?.owner) q.set("owner", filter.owner);
  if (filter?.state) q.set("state", filter.state);
  const qs = q.toString();
  return adminCall<RequestDoc[]>(`/admin/requests${qs ? `?${qs}` : ""}`, { method: "GET", token });
}

/** `quota` is the operator's edited grant on a quota decision; `resolution` is REQUIRED on an
 *  `other` approve (there is nothing else for that approve to do) and optional elsewhere. `note`
 *  is required on every deny. */
export function adminDecideRequest(
  id: string,
  decision: "approve" | "deny",
  body: { note?: string; quota?: Partial<Record<QuotaDim, number>>; resolution?: string },
  token: string,
) {
  return adminCall<RequestDoc>(`/admin/requests/${encodeURIComponent(id)}/${decision}`, {
    method: "POST",
    token,
    body: JSON.stringify(body),
  });
}

// ── /admin (a separate host — crates/workspaces/src/api/admin.rs) ──────────

/** The whole queue, every owner — there is no default `owner` filter. `filter.owner`/`filter.state`
 *  are server-side narrowing (Task 2's `?owner=&state=`); anything finer (free text, dimension,
 *  age) stays client-side over the fetched page, per the ladder. */
export function adminListQuotaRequests(token: string, filter?: { owner?: string; state?: string }) {
  const q = new URLSearchParams();
  if (filter?.owner) q.set("owner", filter.owner);
  if (filter?.state) q.set("state", filter.state);
  const qs = q.toString();
  return adminCall<QuotaRequestDoc[]>(`/admin/quota-requests${qs ? `?${qs}` : ""}`, { method: "GET", token });
}

/** `note` required and non-empty (422 otherwise) — a quota write is dangerous per the Global
 *  Constraint, same rule as deny/roll/drain. The api's `WriteQuotaBody` is `{ spec, note }`, the
 *  spec wrapped, so a flat body would parse as a missing spec. */
export function adminWriteQuota(owner: string, spec: Record<QuotaDim, number>, note: string, token: string) {
  return adminCall<Record<QuotaDim, number>>(`/admin/quota/${encodeURIComponent(owner)}`, {
    method: "PUT",
    token,
    body: JSON.stringify({ spec, note }),
  });
}
