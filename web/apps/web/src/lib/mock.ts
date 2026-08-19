/** Mock data, shaped exactly like the read API's responses so swapping in the real
 *  client is a change of import, not a change of component. */

export type Repo = {
  name: string;
  visibility: "public" | "private";
  /** The team's declarations live in three repos named for what they hold. They
   *  are ordinary repos — clone, branch, review — with a fixed role. */
  system?: "workspaces" | "environments" | "actions";
  description: string;
  pipeline: "passing" | "failing" | "none";
  updated: string;
  head: string;
};

export const REPOS: Repo[] = [
  { name: ".workspaces", system: "workspaces", visibility: "private", description: "Workspace definitions for the team: images, tools, agents.", pipeline: "passing", updated: "4 hours ago", head: "a91c3e0" },
  { name: ".environments", system: "environments", visibility: "private", description: "Environment definitions: what runs where, and what it tracks.", pipeline: "passing", updated: "18 minutes ago", head: "7d20f4b" },
  { name: ".actions", system: "actions", visibility: "private", description: "CI triggers and pipelines for every repo in the team.", pipeline: "failing", updated: "3 hours ago", head: "c0ffee1" },
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

export type SshKey = { id: string; title: string; fingerprint: string; type: "ed25519" | "rsa"; added: string; lastUsed: string };
export const SSH_KEYS: SshKey[] = [
  { id: "k1", title: "MacBook Pro", fingerprint: "SHA256:uV1p8m2Y0aQ7f9sZk3xLw6nB4cD5eF8gH9jK0lM1nO2", type: "ed25519", added: "Mar 2024", lastUsed: "2 hours ago" },
  { id: "k2", title: "kloudlite workspace", fingerprint: "SHA256:aB3cD4eF5gH6iJ7kL8mN9oP0qR1sT2uV3wX4yZ5aB6c", type: "ed25519", added: "Jun 2025", lastUsed: "yesterday" },
];

export type AccessToken = { id: string; name: string; scopes: string[]; created: string; lastUsed: string; expires: string };
export const TOKENS: AccessToken[] = [
  { id: "t1", name: "ci-runner", scopes: ["repo:read", "packages:write"], created: "Jan 2025", lastUsed: "18m ago", expires: "Jan 2026" },
  { id: "t2", name: "laptop-cli", scopes: ["repo:write"], created: "May 2025", lastUsed: "3 days ago", expires: "never" },
];

/** Team-level views read from the three team repos. Each item knows which repo
 *  and file declared it — that file is where you go to change it. */
export type Declared = { repo: string; path: string };

export type Environment = { name: string; source: Declared; tracks: string; sha: string; healthy: boolean; when: string; url?: string };
export const TEAM_ENVIRONMENTS: Environment[] = [
  { name: "production", source: { repo: ".environments", path: "rustic/production.yaml" }, tracks: "tags v*", sha: "0b772db", healthy: true, when: "18m ago", url: "https://git.kloudlite.io" },
  { name: "staging", source: { repo: ".environments", path: "rustic/staging.yaml" }, tracks: "main", sha: "3161493", healthy: true, when: "4h ago", url: "https://staging.git.kloudlite.io" },
  { name: "preview-142", source: { repo: ".environments", path: "web/preview.yaml" }, tracks: "pull requests", sha: "e77c0a9", healthy: false, when: "2d ago" },
];

export type Trigger = { name: string; source: Declared; on: string; last: { status: "passing" | "failing" | "running"; when: string; duration: string } };
export const TRIGGERS: Trigger[] = [
  { name: "ci", source: { repo: ".actions", path: "rustic/ci.yaml" }, on: "push · pull_request", last: { status: "passing", when: "2h ago", duration: "4m 12s" } },
  { name: "release", source: { repo: ".actions", path: "rustic/release.yaml" }, on: "tag v*", last: { status: "passing", when: "yesterday", duration: "9m 55s" } },
  { name: "nightly", source: { repo: ".actions", path: "rustic/nightly.yaml" }, on: "schedule 02:00 UTC", last: { status: "passing", when: "9h ago", duration: "6m 40s" } },
  { name: "ci", source: { repo: ".actions", path: "web/ci.yaml" }, on: "push · pull_request", last: { status: "running", when: "just now", duration: "1m 08s" } },
  { name: "plan", source: { repo: ".actions", path: "infra/plan.yaml" }, on: "pull_request", last: { status: "failing", when: "3h ago", duration: "2m 41s" } },
];

/** A workspace *session*: a definition from `.workspaces`, brought up on a repo at
 *  a ref, for a person or an agent. This is what the Workspaces page lists; the
 *  definitions are the templates it starts from. */
export type WorkspaceSession = {
  id: string;
  definition: string;
  repo: string;
  ref: string;
  owner: { kind: "user"; login: string; name: string } | { kind: "agent"; name: string; for: string };
  status: "running" | "idle" | "stopped";
  agents: number;
  started: string;
  active: string;
  cpu?: string;
  /** Set when this workspace was cloned from another — the usual case for an agent's:
   *  it starts from a person's workspace, with their branch and state, and works beside it. */
  forkedFrom?: string;
};

export const WORKSPACE_SESSIONS: WorkspaceSession[] = [
  { id: "ws-01", definition: "rust-dev", repo: "rustic", ref: "feat/browse-api", owner: { kind: "user", login: "karthik", name: "Karthik Thirumalasetti" }, status: "running", agents: 2, started: "4h ago", active: "just now", cpu: "2.1 / 4 vCPU" },
  { id: "ws-01a", definition: "rust-dev", repo: "rustic", ref: "feat/browse-api", owner: { kind: "agent", name: "test-writer", for: "karthik" }, status: "running", agents: 0, started: "1h ago", active: "just now", forkedFrom: "ws-01" },
  { id: "ws-01b", definition: "rust-dev", repo: "rustic", ref: "feat/browse-api-docs", owner: { kind: "agent", name: "doc-writer", for: "karthik" }, status: "idle", agents: 0, started: "2h ago", active: "40m ago", forkedFrom: "ws-01" },
  { id: "ws-03", definition: "rust-dev", repo: "rustic", ref: "main", owner: { kind: "agent", name: "nightly-fixer", for: "karthik" }, status: "running", agents: 0, started: "12m ago", active: "just now", cpu: "3.6 / 4 vCPU" },
  { id: "ws-04", definition: "web", repo: "web", ref: "feat/feed", owner: { kind: "user", login: "alice", name: "Alice Chen" }, status: "idle", agents: 1, started: "yesterday", active: "3h ago" },
  { id: "ws-04a", definition: "web", repo: "web", ref: "feat/feed", owner: { kind: "agent", name: "reviewer", for: "alice" }, status: "running", agents: 0, started: "38m ago", active: "2m ago", forkedFrom: "ws-04" },
  { id: "ws-05", definition: "infra", repo: "infra", ref: "main", owner: { kind: "user", login: "bob", name: "Bob Osei" }, status: "stopped", agents: 0, started: "3d ago", active: "3d ago" },
];

export type WorkspaceDefinition = { name: string; path: string; image: string; tools: string[]; sessions: number };
export const WORKSPACE_DEFINITIONS: WorkspaceDefinition[] = [
  { name: "rust-dev", path: "rust-dev.yaml", image: "rust:1.80", tools: ["sccache", "cargo-nextest", "clippy"], sessions: 3 },
  { name: "web", path: "web.yaml", image: "bun:1.3", tools: ["node:22", "playwright"], sessions: 1 },
  { name: "infra", path: "infra.yaml", image: "terraform:1.9", tools: ["kubectl", "az"], sessions: 1 },
];
