import type { SnapshotState } from "@/lib/snapshot-state";
import type { QuotaDim, QuotaReport } from "@/lib/quota";
import { call } from "./client";

/**
 * Workspaces, environments, volumes and quota.
 */
/** Mirrors `crates/workspaces/src/model.rs::WsState` — lowercase on the wire. */
export type WsState = "creating" | "ready" | "stopped" | "error" | "deleted";
export type EnvState = "creating" | "running" | "stopped" | "error" | "deleted";

export type ApiWorkspace = {
  id: string;
  owner: string;
  /** Empty for personal. */
  team: string;
  name: string;
  region: string;
  state: WsState;
  /** The container image `ws-{id}` runs, the platform image unless set at create — the tools come from Nix. */
  image: string;
  placement: string | null;
  volume: string | null;
  quota_gb: number;
  /** nixpkgs attribute names the workspace declares — what was ASKED for, not what is installed. */
  packages: string[];
  /** The platform's base set every workspace gets on top of its own list; shown, not edited. */
  base_packages?: string[];
  /** The `PackagesReady` condition; absent until the reconciler has reported on the list. */
  packages_status?: { ready: boolean; reason: string; message: string } | null;
  /** What each `attr@version` entry in `packages` resolved to. Desired state, written by the
   *  api when it resolved the pin — a bare entry has no lock, so this is never 1:1 with
   *  `packages`. `source` is `crd::LockSource` (serde lowercase) — which index answered, and
   *  therefore whether a store path came with it. */
  locks?: { entry: string; version: string; rev: string; source: "nixhub" | "mirror" }[];
  /** `owner/name` and branch the workspace was seeded from; absent for one created empty,
   *  cloned or restored. */
  repo?: string;
  branch?: string;
  /** Present once the workspace has an sshd with a host key — i.e. once it can be reached.
   *  Absent while it is coming up, and for a stopped one. */
  ssh?: { gateway: string; host_key: string } | null;
  /** The `Replicated` condition, verbatim from the node that wrote it — "safe to start anywhere"
   *  vs "still copying". Absent while running: it is only computed for a stopped parent. */
  replicated?: { ready: boolean; reason: string; message: string } | null;
  /** `Degraded/NodeDead` — the source's node is down, so a start is refused and a clone is the way on. */
  degraded?: { ready: boolean; reason: string; message: string } | null;
  /** `Decommissioning/NodeLeaving` — the node is being retired; the next start lands elsewhere. */
  decommissioning?: { ready: boolean; reason: string; message: string } | null;
  /** Sent only when `Placed` is false — `NoCapacity` means no node has room for it yet. */
  placed?: { ready: boolean; reason: string; message: string } | null;
  /** What a clone was grafted onto, and whether that cut predates the source's node going down.
   *  Only a clone response carries it — an environment clone never does. */
  based_on?: { snapshot: string; at?: string | null; age_seconds: number; interrupted: boolean } | null;
};

export type ApiMount = { folder: string; path: string };
/** `model::Service`. `ports` is `#[serde(default)]` on the Rust side, so an environment document
 *  written before ports existed deserializes as an empty list — the wire always carries the key. */
export type ApiService = {
  name: string;
  image: string;
  command: string[];
  env: Record<string, string>;
  mounts: ApiMount[];
  ports: number[];
};

/** One of the service's declared ports, and the port on the workspace that answers it. */
export type ApiInterceptPort = { service: number; workspace: number };

export type ApiEnvironment = {
  id: string;
  owner: string;
  name: string;
  region: string;
  state: EnvState;
  placement: string | null;
  volume: string | null;
  services: ApiService[];
  /** The WISH, `EnvironmentSpec.intercepts`, written only by `/v1`. What is in force is
   *  `service_status` below. Absent on an environment stored before intercepts existed. */
  intercepts?: { service: string; workspace: string; ports: ApiInterceptPort[] }[];
  /** STATUS: what the agent observed, one entry per service, matched to `services` BY NAME.
   *  `intercepted_by` is the workspace actually receiving that service's traffic. The wish above
   *  and this are separate facts and neither is ever inferred from the other: a wish with nothing
   *  in force means the intercepting workspace is stopped and the real service is answering.
   *  Absent while the environment has no status yet. */
  service_status?: { name: string; ready: boolean; message?: string | null; intercepted_by?: string | null }[];
  /** The snapshot the volume last landed on, when an in-place restore put one there — only
   *  `GET /v1/environments/{id}` fills it in. Absent means "current" is simply the newest record. */
  restored_to?: string | null;
  /** When that restore was asked for: a record pushed after it, descending from `restored_to`,
   *  is where the environment has moved on to; one from before is a sibling branch. */
  restore_requested_at?: string | null;
  /** Why this environment is mid-restore (`Draining`, `Restoring`, `Requested`), or absent. */
  restoring?: string | null;
  /** The `Replicated` condition, verbatim from the node that wrote it — "safe to start anywhere"
   *  vs "still copying". Absent while running: it is only computed for a stopped parent. */
  replicated?: { ready: boolean; reason: string; message: string } | null;
  /** `Degraded/NodeDead` — the source's node is down, so a start is refused and a clone is the way on. */
  degraded?: { ready: boolean; reason: string; message: string } | null;
  /** `Decommissioning/NodeLeaving` — the node is being retired; the next start lands elsewhere. */
  decommissioning?: { ready: boolean; reason: string; message: string } | null;
  /** Sent only when `Placed` is false — `NoCapacity` means no node has room for it yet. */
  placed?: { ready: boolean; reason: string; message: string } | null;
};

/** The caller's workspaces in `team`, or their personal ones when it is absent or their own
 *  handle. A team page never shows personal work and the personal page never shows a team's:
 *  each (team, person) pair is its own namespace on the cluster. */
export function listWorkspaces(token: string, team?: string) {
  const q = team ? `?team=${encodeURIComponent(team)}` : "";
  return call<ApiWorkspace[]>(`/v1/workspaces${q}`, { method: "GET", token });
}

/** `crates/workspaces/src/api.rs::NewWorkspace`. `repo`/`branch` come as a pair — the api
 *  refuses a repo without a branch, since "the default branch" is a different workspace
 *  depending on when it was made. */
export function createWorkspace(
  token: string,
  body: {
    team?: string;
    name: string;
    region: string;
    quota_gb: number;
    image?: string;
    repo?: string;
    branch?: string;
    packages?: string[];
  },
) {
  return call<ApiWorkspace>("/v1/workspaces", { method: "POST", token, body: JSON.stringify(body) });
}

/** Replace the declared package list. The whole list, not a delta: the api merge-patches
 *  `spec.packages` with exactly what is sent. */
export function setWorkspacePackages(token: string, id: string, packages: string[]) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}`, {
    method: "PATCH",
    token,
    body: JSON.stringify({ packages }),
  });
}

/** Re-resolve every pinned entry against the package index and return the workspace with its
 *  new `locks`. The list itself is untouched — only what the pins point at moves. */
export function updateWorkspacePackages(token: string, id: string) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}/packages/update`, {
    method: "POST",
    token,
    body: "{}",
  });
}

/** `crates/workspaces/src/model.rs::Region`, narrowed to what the app reads (`listRegions`'s only
 *  caller uses just `status` and `id`); `name`, `storage_account`, `blob_container` have no
 *  reader anywhere. */
export type ApiRegion = { id: string; status: string };

export function listRegions(token: string) {
  return call<ApiRegion[]>("/v1/regions", { method: "GET", token });
}

/** Attach a workspace to one environment: its services resolve by bare name from then on.
 *  409 when the environment is in another region; the message says so. */
export function attachWorkspace(token: string, id: string, environment: string) {
  return call<void>(`/v1/workspaces/${encodeURIComponent(id)}/attach`, {
    method: "POST", token, body: JSON.stringify({ environment }),
  });
}

/** Deliver the environment's traffic for one service to an attached workspace instead. 202 with
 *  the environment; the controller stops the real service and points its endpoints at the
 *  workspace pod. Refusals are one sentence: 404 unknown service or workspace, 409 not attached /
 *  not running / already intercepted (naming the holder), 422 a port the service does not
 *  declare or named twice. A `ports` entry may be omitted — the same number answers it. */
export function setIntercept(
  token: string,
  id: string,
  body: { service: string; workspace: string; ports: ApiInterceptPort[] },
) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}/intercepts`, {
    method: "POST", token, body: JSON.stringify(body),
  });
}

/** Drop the wish. The ONLY thing that does: stopping, detaching or deleting the workspace leaves
 *  it in place so the intercept takes hold again by itself. Idempotent. */
export function clearIntercept(token: string, id: string, service: string) {
  return call<void>(
    `/v1/environments/${encodeURIComponent(id)}/intercepts/${encodeURIComponent(service)}`,
    { method: "DELETE", token },
  );
}

export function listEnvironments(token: string, owner?: string) {
  const qs = owner ? `?owner=${encodeURIComponent(owner)}` : "";
  return call<ApiEnvironment[]>(`/v1/environments${qs}`, { method: "GET", token });
}

/** One environment, by id. 404 when it is gone — which is exactly what an ARCHIVED row is, so
 *  the environment page falls back to the volume's snapshot records for its name and services. */
export function getEnvironment(token: string, id: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}`, { method: "GET", token });
}

/** Snapshot + upload + register, atomically. The answer is only the REQUEST's id: the snapshot
 *  record appears in the volume's history when the push lands, which is what the page polls for. */
export function pushEnvironment(token: string, id: string, message?: string) {
  return call<{ id: string }>(`/v1/environments/${encodeURIComponent(id)}/push`, {
    method: "POST",
    token,
    body: message ? JSON.stringify({ message }) : undefined,
  });
}

/** Put a past snapshot back into THIS environment's own volume, rather than into a new one.
 *  202 with nothing to read: the controllers scale the services down, swap the subvolume and
 *  bring them back, and the environment's own state is where that progress shows. */
export function restoreEnvironmentInPlace(token: string, id: string, snapshotId: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}/restore-in-place`, {
    method: "POST",
    token,
    body: JSON.stringify({ snapshot_id: snapshotId }),
  });
}

export function deleteWorkspace(token: string, id: string) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

export function deleteEnvironment(token: string, id: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}`, { method: "DELETE", token });
}

/** The one mutating verb: snapshot + upload + register, atomically. `message` is optional. */
export function pushWorkspace(token: string, id: string, message?: string) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}/push`, {
    method: "POST",
    token,
    body: message ? JSON.stringify({ message }) : undefined,
  });
}

/** The one local-copy verb — the server picks `clone_local` vs `clone_running` itself,
 *  keyed on whether the source's container is running. */
export function cloneWorkspace(token: string, id: string, name: string) {
  return call<ApiWorkspace>(`/v1/workspaces/${encodeURIComponent(id)}/clone`, {
    method: "POST",
    token,
    body: JSON.stringify({ name }),
  });
}

/** Same one local-copy verb as `cloneWorkspace`, for an environment — pauses its compose
 *  project (not a single container) around the copy. */
export function cloneEnvironment(token: string, id: string, name: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}/clone`, {
    method: "POST",
    token,
    body: JSON.stringify({ name }),
  });
}

/** New workspace grafted onto an explicit past snapshot, not the source's
 *  current tip — see `crates/workspaces/src/api.rs::restore_ws`. */
export function restoreWorkspace(
  token: string,
  name: string,
  snapshotId: string,
  // Each field OVERRIDES the snapshot's own frozen definition, so an omitted one must stay off
  // the wire entirely — sending `image: undefined`'s JSON hole would read as "no image".
  extra?: { image?: string; packages?: string[] },
) {
  return call<ApiWorkspace>(`/v1/workspaces/restore`, {
    method: "POST",
    token,
    // The snapshot id is enough: the api tier finds the volume it belongs to. No source
    // workspace is named, because a restore is most wanted when there no longer is one.
    body: JSON.stringify({
      name,
      snapshot_id: snapshotId,
      ...(extra?.image ? { image: extra.image } : {}),
      ...(extra?.packages !== undefined ? { packages: extra.packages } : {}),
    }),
  });
}

/** New environment grafted onto a past snapshot — `restore_env`'s twin of `restoreWorkspace`.
 *  A snapshot freezes the service list beside the data, so omitting `services` restores what was
 *  pushed; an empty list means the same on the server. Only a non-empty list overrides. */
export function restoreEnvironment(token: string, name: string, snapshotId: string, services?: ApiService[]) {
  return call<ApiEnvironment>(`/v1/environments/restore`, {
    method: "POST",
    token,
    body: JSON.stringify({ name, snapshot_id: snapshotId, ...(services ? { services } : {}) }),
  });
}

export function startWorkspace(token: string, id: string) {
  return call<void>(`/v1/workspaces/${encodeURIComponent(id)}/start`, { method: "POST", token });
}

export function stopWorkspace(token: string, id: string) {
  return call<{ warning?: string }>(`/v1/workspaces/${encodeURIComponent(id)}/stop`, { method: "POST", token });
}

export function startEnvironment(token: string, id: string) {
  return call<ApiEnvironment>(`/v1/environments/${encodeURIComponent(id)}/start`, { method: "POST", token });
}

export function stopEnvironment(token: string, id: string) {
  return call<ApiEnvironment & { warning?: string }>(`/v1/environments/${encodeURIComponent(id)}/stop`, { method: "POST", token });
}

/** `crates/workspaces/src/api.rs::VolumeSummary` — one row per VOLUME that has ever been
 *  pushed, read from the server tier's registry rather than from live workspaces. A snapshot
 *  outlives the thing it was taken of, so a row can name a source that no longer exists. */
export type ApiVolumeSummary = {
  name: string;
  kind: "workspace" | "environment";
  volume: string | null;
  /** What the source was called; the volume id when a record carries no provenance. */
  display_name: string;
  /** The workspace/environment is gone. The snapshots are not. */
  deleted: boolean;
  /** How many PUSHES are on this volume — the only thing keeping it once its workspace or
   *  environment is gone. Sync points are not counted; they are never shown. */
  snapshots: number;
  /** RFC3339 of the newest push; `null` while the only push is still being taken. */
  last_push_at: string | null;
};

/** `kind` narrows to `workspace` or `environment`. The Environments page asks for `environment`
 *  to find its ARCHIVED rows — volumes with snapshots and no live environment left. A workspace's
 *  snapshots are that one person's undo history and are reached only from their own row. */
export function listVolumes(token: string, kind?: "workspace" | "environment", owner?: string) {
  const qs = new URLSearchParams();
  if (kind) qs.set("kind", kind);
  // A team's page must show that team's archived rows and not the caller's personal ones — the
  // same filter `listEnvironments` passes, for the same reason.
  if (owner) qs.set("owner", owner);
  const q = qs.toString();
  return call<ApiVolumeSummary[]>(`/v1/volumes${q ? `?${q}` : ""}`, { method: "GET", token });
}

/** `crates/workspaces/src/api.rs::snapshot_rows` — the volume's SNAPSHOTS, newest
 *  first. Sync points are internal and never appear here. The row also carries
 *  `phase`, left undeclared here — no reader in this app looks at it yet; add it if one needs to.
 *  The wire also carries `lineage` (always `[]`) and `region` (always `""`), left undeclared for
 *  the same reason: nothing reads them, and a type that names a field invites one to. */
export type ApiCommitRecord = {
  id: string;
  /** The definition frozen at push time — `null` for snapshots taken before it was recorded. */
  state: SnapshotState | null;
  /** The snapshot this one was pushed on top of — derived server-side from the blob chain. A push
   *  after an in-place restore grafts onto the restored record, which is what makes a branch. */
  parent?: string | null;
  message?: string;
  /** RFC3339. camelCase because `/history` builds its rows by hand rather than serializing
   *  `CommitRecord` (`crates/workspaces/src/api.rs:2027`); `null` when the object carries no
   *  creation timestamp. Read it through `snapshotTime` (`lib/snapshot.ts`), never by hand. */
  createdAt: string | null;
};

/** Deletes the volume and every `Snapshot` on it. A volume's snapshots are
 *  the only thing keeping it once its workspace or environment is gone, so this is what finally
 *  removes it: the Snapshots section's own "Delete volume". The bytes go with it — each node
 *  holding the subvolume drops it on its next beat. 409 while a working copy still uses it. */
export function deleteVolume(token: string, name: string) {
  return call<void>(`/v1/volumes/${encodeURIComponent(name)}`, { method: "DELETE", token });
}

/** Drops ONE snapshot from a volume's lineage. The environment's disk is untouched — this
 *  removes the record, not the data it points at. */
export function deleteVolumeSnapshot(token: string, name: string, snapshot: string) {
  return call<void>(
    `/v1/volumes/${encodeURIComponent(name)}/snapshots/${encodeURIComponent(snapshot)}`,
    { method: "DELETE", token },
  );
}

export function volumeHistory(token: string, name: string) {
  return call<ApiCommitRecord[]>(`/v1/volumes/${encodeURIComponent(name)}/history`, { method: "GET", token });
}

/** An owner's ceiling and what is against it. Computed by the api on every request — there is no
 *  cached number to be stale. */
export function getQuota(owner: string, token: string) {
  return call<QuotaReport>(`/v1/quota?owner=${encodeURIComponent(owner)}`, { method: "GET", token });
}

export type QuotaRequestDoc = {
  id: string;
  owner: string;
  requested: Partial<Record<QuotaDim, number>>;
  reason: string;
  state: "pending" | "approved" | "denied";
  decidedBy?: string | null;
  decidedAt?: string | null;
  note?: string | null;
  createdAt?: string | null;
};

export type { RequestDoc, RequestKind } from "@/lib/requests";
