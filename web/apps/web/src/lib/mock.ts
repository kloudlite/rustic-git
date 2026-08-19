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

/** The team feed: everything that happened across the org, newest first, with
 *  enough detail that a reader can act without opening each item. */
export type FeedEvent = {
  id: string;
  kind: "push" | "deploy" | "pipeline" | "release" | "workspace" | "environment";
  actor: { name: string; login: string } | null; // null: the system did it
  repo: string;
  ref?: string;
  title: string;
  when: string;
  day: "Today" | "Yesterday" | "Earlier this week";
  ok?: boolean;
  commits?: { sha: string; message: string }[];
  detail?: string;
};

export const FEED: FeedEvent[] = [
  { id: "1", kind: "deploy", actor: null, repo: "rustic", ref: "main", title: "Deployed 0b772db to production", when: "18m", day: "Today", ok: true, detail: "Rolled out in 46s · 3 replicas healthy" },
  { id: "2", kind: "push", actor: { name: "Alice Chen", login: "alice" }, repo: "rustic", ref: "main", title: "pushed 3 commits", when: "2h", day: "Today",
    commits: [
      { sha: "15da845", message: "Cap the push body below the memory cliff" },
      { sha: "36f711e", message: "Do not hand a repo to a node that is leaving" },
      { sha: "e024dea", message: "Plan: implement object serving in five tasks" },
    ] },
  { id: "3", kind: "pipeline", actor: null, repo: "infra", ref: "main", title: "Pipeline failed", when: "3h", day: "Today", ok: false, detail: "terraform-plan · step \"plan\" exited 1" },
  { id: "4", kind: "workspace", actor: { name: "Karthik Thirumalasetti", login: "karthik" }, repo: "web", ref: "feat/feed", title: "opened a workspace", when: "4h", day: "Today", detail: "forked from main · 2 agents attached" },
  { id: "5", kind: "release", actor: { name: "Alice Chen", login: "alice" }, repo: "rustic", ref: "v1.0", title: "released v1.0", when: "5h", day: "Today", detail: "tagged from main at 15da845" },
  { id: "6", kind: "environment", actor: { name: "Bob Osei", login: "bob" }, repo: "rustic", ref: "staging", title: "forked staging into staging-bob", when: "9h", day: "Today", detail: "snapshot 3161493 · switched in 1.2s" },
  { id: "7", kind: "push", actor: { name: "Bob Osei", login: "bob" }, repo: "kolomi-ws", ref: "main", title: "pushed 1 commit", when: "yesterday", day: "Yesterday",
    commits: [{ sha: "9c11f02", message: "Pin the base image for the rust workspace" }] },
  { id: "8", kind: "deploy", actor: null, repo: "rustic", ref: "main", title: "Deployed 3161493 to staging", when: "yesterday", day: "Yesterday", ok: true, detail: "Rolled out in 41s" },
  { id: "9", kind: "pipeline", actor: null, repo: "web", ref: "main", title: "Pipeline passed", when: "2d", day: "Earlier this week", ok: true, detail: "build · test · image e77c0a9" },
  { id: "10", kind: "push", actor: { name: "Karthik Thirumalasetti", login: "karthik" }, repo: "infra", ref: "main", title: "pushed 2 commits", when: "3d", day: "Earlier this week",
    commits: [
      { sha: "4ab7d31", message: "Network policy for the api deployment" },
      { sha: "0d1e9c2", message: "Secrets layout: one key per credential" },
    ] },
];

export type Member = { name: string; login: string; email: string; role: "owner" | "admin" | "member"; joined: string };

export const MEMBERS: Member[] = [
  { name: "Karthik Thirumalasetti", login: "karthik", email: "karthik@kloudlite.io", role: "owner", joined: "Jan 2024" },
  { name: "Alice Chen", login: "alice", email: "alice@kloudlite.io", role: "admin", joined: "Mar 2024" },
  { name: "Bob Osei", login: "bob", email: "bob@kloudlite.io", role: "member", joined: "Jun 2025" },
];
