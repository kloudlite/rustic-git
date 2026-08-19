/** Mock data, shaped exactly like the read API's responses so swapping in the real
 *  client is a change of import, not a change of component. */

export type Repo = {
  name: string;
  visibility: "public" | "private";
  description: string;
  pipeline: "passing" | "failing" | "none";
  updated: string;
  head: string;
};

export const REPOS: Repo[] = [
  { name: "rustic", visibility: "public", description: "Source hosting that stores packs in object storage and refs in an embedded database.", pipeline: "passing", updated: "2 hours ago", head: "15da845" },
  { name: "kolomi-ws", visibility: "private", description: "Workspace definitions and environment manifests for the platform.", pipeline: "passing", updated: "yesterday", head: "9c11f02" },
  { name: "infra", visibility: "private", description: "Cluster bootstrap, network policies and secrets layout.", pipeline: "failing", updated: "3 days ago", head: "4ab7d31" },
  { name: "web", visibility: "private", description: "The platform frontend, built with Next.js server components.", pipeline: "passing", updated: "last week", head: "e77c0a9" },
];

export type Activity = {
  kind: "push" | "deploy" | "pipeline" | "release";
  repo: string;
  summary: string;
  detail: string;
  when: string;
  ok?: boolean;
};

export const ACTIVITY: Activity[] = [
  { kind: "deploy", repo: "rustic", summary: "Deployed to production", detail: "0b772db", when: "18m", ok: true },
  { kind: "push", repo: "rustic", summary: "alice pushed 3 commits to main", detail: "15da845", when: "2h" },
  { kind: "pipeline", repo: "infra", summary: "Pipeline failed on main", detail: "terraform-plan", when: "3h", ok: false },
  { kind: "release", repo: "rustic", summary: "Released v1.0", detail: "tagged from main", when: "5h" },
  { kind: "push", repo: "kolomi-ws", summary: "bob pushed 1 commit to main", detail: "9c11f02", when: "yesterday" },
];

export const ENVIRONMENTS = [
  { name: "production", repo: "rustic", sha: "0b772db", healthy: true, when: "18m ago" },
  { name: "staging", repo: "rustic", sha: "3161493", healthy: true, when: "4h ago" },
  { name: "preview-142", repo: "web", sha: "e77c0a9", healthy: false, when: "2d ago" },
];
