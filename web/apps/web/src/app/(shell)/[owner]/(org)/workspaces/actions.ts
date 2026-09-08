"use server";

import { revalidatePath } from "next/cache";
import { redirect } from "next/navigation";
import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
// `owner` reaches every action below as FormData, and goes straight into a revalidatePath
// PATTERN. A segment carrying `/` or `..` would silently revalidate something else, so each
// action refuses it — a bad one is never a real submission, since the pages that render these
// forms fill the field from the route params.
import { safeSegment } from "@/lib/slug";
import { cloneResult } from "@/lib/ws-status";
import { getSession } from "@/lib/session";
import { packageListError, packagesField } from "@/lib/packages-field";
import { dimFromRefusal, type QuotaDim } from "@/lib/quota";

/** `ok` is what lets a dialog close on success — see `useDialogUntilSuccess`. */
/** `warning`: the api's sentence when a stop moves a workspace off a dead node — edits since the
 *  last sync point stay on that node. Not an error: the stop happened, the person should know. */
/** `basedOn`: the sentence naming the cut a clone was grafted onto. A success the dialog must
 *  SHOW rather than close on — it is the only place that cut is ever named. */
/** `quotaDim`: set only when `error` is the quota 409 — the dimension it named, so the form can
 *  offer the request dialog pre-filled rather than just showing the api's sentence. */
export type WsActionState = { ok?: true; error?: string; warning?: string; basedOn?: string; quotaDim?: QuotaDim } | null;

/** Mutations are async jobs (202 + a doc whose `state` is still `creating`), so
 *  there is nothing to poll here: revalidating just re-renders the list with
 *  whatever state the api already wrote, same as every other list in the app. */
export async function pushWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const message = String(formData.get("message") ?? "").trim();

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.pushWorkspace(token, id, message || undefined);
  if (!r.ok) return { error: r.message || "Could not push." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function cloneWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the clone." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.cloneWorkspace(token, id, name);
  if (!r.ok) return { error: r.message || "Could not clone." };
  revalidatePath(`/${owner}/workspaces`);
  return cloneResult(r.value);
}

export async function restoreWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const snapshotId = String(formData.get("snapshotId") ?? "");
  // Empty when the dialog's snapshot list has not landed, or landed empty. The disabled submit
  // button is a convenience; this is the check.
  if (!snapshotId) return { error: "Choose a snapshot to restore." };
  const name = String(formData.get("name") ?? "").trim();
  if (!name) return { error: "Name the new workspace." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  // Present only when the snapshot carried a definition to pre-fill from; absent fields let the
  // api use the snapshot's own, which is also what an untouched form sends back.
  const image = String(formData.get("image") ?? "").trim();
  const packages = packagesField(formData);

  const r = await api.restoreWorkspace(token, name, snapshotId, { image: image || undefined, packages });
  if (!r.ok) return { error: r.message || "Could not restore." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function startWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.startWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not start." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

export async function stopWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.stopWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not stop." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true, warning: r.value?.warning };
}

export async function deleteWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteWorkspace(token, id);
  if (!r.ok) return { error: r.message || "Could not delete." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

/** One snapshot out of a volume's lineage, from that workspace's Snapshots page. Deletes the
 *  snapshot itself — a snapshot is kept until it is explicitly deleted, and this is the explicit
 *  delete. The workspace's own disk, if it still exists, is untouched. */
export async function deleteWorkspaceSnapshot(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That workspace is not valid." };
  const snapshotId = String(formData.get("snapshotId") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteVolumeSnapshot(token, id, snapshotId);
  if (!r.ok) return { error: r.message || "Could not delete the snapshot." };
  revalidatePath(`/${owner}/workspaces/${id}/snapshots`);
  return { ok: true };
}

/** The Snapshots section's own delete, for a workspace that is already gone: its snapshots are
 *  the only thing keeping the volume, so dropping them drops it. `deleteEnvironmentSnapshots`
 *  is the same action for the other kind. */
export async function deleteWorkspaceSnapshots(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = safeSegment(String(formData.get("id") ?? ""));
  if (!id) return { error: "That workspace is not valid." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.deleteVolume(token, id);
  if (!r.ok) return { error: r.message || "Could not delete the snapshots." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

/** "Open in a workspace", from the repo Clone menu and the PR header. The person names the
 *  workspace and may attach an environment; the backend does the rest — the pod's init container
 *  clones the repo over SSH with the owner's platform key. Nothing is reused by name: a name that
 *  is taken is the api's 409, shown as it is, and the person picks another. */
export async function openInWorkspace(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  const repo = safeSegment(String(formData.get("repo") ?? ""));
  // A branch is not a path segment — it legitimately carries `/` — so it gets its own rule
  // rather than `safeSegment`. It never reaches revalidatePath; it only goes to the api.
  const branch = String(formData.get("branch") ?? "").trim();
  if (!owner || !repo) return { error: "That repository name is not valid." };
  if (!branch || branch.includes("..") || branch.startsWith("-")) return { error: "That branch name is not valid." };
  const name = String(formData.get("name") ?? "").trim().toLowerCase();
  if (!/^[a-z0-9][a-z0-9-]{0,39}$/.test(name)) return { error: "A name is letters, digits and dashes, up to 40." };
  const environment = String(formData.get("environment") ?? "").trim();
  if (environment && !safeSegment(environment)) return { error: "That environment is not valid." };

  const token = await tokenOr();
  if (typeof token !== "string") return token;
  const session = await getSession();

  // A repo under your own handle is personal work, not a team's — same rule the api applies.
  const team = session?.user.owner === owner ? undefined : owner;

  const regions = await api.listRegions(token);
  if (!regions.ok) return { error: regions.message || "Could not read the regions." };
  // ponytail: first ACTIVE region; a picker when there is a second. A retired region stays in
  // the list so its old records still resolve, so "first" alone once chose a region with no
  // agents in it and the workspace sat unplaced forever.
  let region = regions.value.find((r) => r.status === "active")?.id;
  if (environment) {
    // An environment is one region's; the workspace has to land beside it or the attach is a
    // 409 after a create nobody wanted. Pick the environment's region when it is active.
    const envs = await api.listEnvironments(token, team);
    if (!envs.ok) return { error: envs.message || "Could not read your environments." };
    const env = envs.value.find((e) => e.id === environment);
    if (!env) return { error: "That environment is not one of yours." };
    if (!regions.value.some((r) => r.id === env.region && r.status === "active")) {
      return { error: `The environment's region (${env.region}) is not active.` };
    }
    region = env.region;
  }
  if (!region) return { error: "No region is available to run a workspace in." };

  const r = await api.createWorkspace(token, {
    team,
    name,
    region,
    quota_gb: 10,
    repo: `${owner}/${repo}`,
    branch,
  });
  if (!r.ok) {
    const dim = r.kind === "conflict" ? dimFromRefusal(r.message) : null;
    return { error: r.message || "Could not open a workspace.", quotaDim: dim ?? undefined };
  }
  if (environment) {
    const a = await api.attachWorkspace(token, r.value.id, environment);
    // The workspace exists either way; say what did not happen rather than hide a made thing.
    if (!a.ok) return { error: `The workspace was created, but attaching failed: ${a.message}`, warning: r.value.id };
  }

  revalidatePath(`/${owner}/workspaces`);
  // Outside every catch above on purpose: redirect works by throwing.
  redirect(`/${owner}/workspaces`);
}

/** The whole package list, replaced. The field is free text (whitespace or commas) because that
 *  is how the names are written down everywhere else — a nixpkgs attribute never contains
 *  either, so the split cannot corrupt one. Validation is the api's; its 422 names the entry. */
export async function setPackages(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");
  const packages = String(formData.get("packages") ?? "")
    .split(/[\s,]+/)
    .filter(Boolean);
  const bad = packageListError(packages);
  if (bad) return { error: bad };

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.setWorkspacePackages(token, id, packages);
  // Verbatim: the api's 422 names the entry and the nearest published versions, and its 503 says
  // the index is down — both are the whole answer, and a house sentence would throw them away.
  if (!r.ok) return { error: r.message || "Could not set the packages." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

/** Re-resolve the pins. The list does not change, so there is no field to validate: this is
 *  "what does `nodejs@20` mean today", and the api's message is again shown verbatim. */
export async function updatePackages(_prev: WsActionState, formData: FormData): Promise<WsActionState> {
  const owner = safeSegment(String(formData.get("owner") ?? ""));
  if (!owner) return { error: "That owner name is not valid." };
  const id = String(formData.get("id") ?? "");

  const token = await tokenOr();
  if (typeof token !== "string") return token;

  const r = await api.updateWorkspacePackages(token, id);
  if (!r.ok) return { error: r.message || "Could not update the pinned packages." };
  revalidatePath(`/${owner}/workspaces`);
  return { ok: true };
}

/** The owner's environments, for the "Open in a workspace" dialog's picker. Read on open, not
 *  on every page that carries the button. */
export async function environmentsFor(owner: string): Promise<{ id: string; name: string; region: string }[]> {
  const safe = safeSegment(owner);
  if (!safe) return [];
  const token = await tokenOr();
  if (typeof token !== "string") return [];
  const session = await getSession();
  const team = session?.user.owner === safe ? undefined : safe;
  const envs = await api.listEnvironments(token, team);
  return envs.ok ? envs.value.map((e) => ({ id: e.id, name: e.name, region: e.region })) : [];
}
