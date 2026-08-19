/** Mock data for one repo, shaped like the browse API (tree/blob/log/commit) plus
 *  the issue, PR and run shapes the product will add. */

export const REPO = {
  owner: "karthik",
  name: "rustic",
  visibility: "public" as const,
  description: "Source hosting that stores packs in object storage and refs in an embedded database.",
  defaultBranch: "main",
  branches: ["main", "feat/browse-api", "fix/force-claim"],
  tags: ["v1.0", "v0.9.2"],
  head: { sha: "15da845", message: "Cap the push body below the memory cliff", author: "alice", when: "2 hours ago" },
};

export type TreeEntry = { name: string; kind: "dir" | "file"; message: string; when: string };

export const TREE: Record<string, TreeEntry[]> = {
  "": [
    { name: "deploy", kind: "dir", message: "Retry a release: an unreleased lease costs a full TTL", when: "3 days ago" },
    { name: "docs", kind: "dir", message: "Design: clones answered with signed URLs", when: "5 days ago" },
    { name: "src", kind: "dir", message: "Cap the push body below the memory cliff", when: "2 hours ago" },
    { name: "tests", kind: "dir", message: "Do not hand a repo to a node that is leaving", when: "yesterday" },
    { name: ".gitignore", kind: "file", message: "Initial commit", when: "4 months ago" },
    { name: "Cargo.toml", kind: "file", message: "SSPL-1.0", when: "2 weeks ago" },
    { name: "LICENSE", kind: "file", message: "SSPL-1.0", when: "2 weeks ago" },
    { name: "README.md", kind: "file", message: "Document the peer-only browse endpoints", when: "6 days ago" },
  ],
  src: [
    { name: "http", kind: "dir", message: "Route the browse API on the peer router only", when: "6 days ago" },
    { name: "api.rs", kind: "file", message: "Decode once, then judge: parsed path drives authz and cache key", when: "5 days ago" },
    { name: "auth.rs", kind: "file", message: "Public repos are readable by anyone signed in", when: "5 days ago" },
    { name: "browse.rs", kind: "file", message: "Blob bytes serialise as base64", when: "6 days ago" },
    { name: "cache.rs", kind: "file", message: "generation() defaults to 0 so the first purge orphans", when: "5 days ago" },
    { name: "http.rs", kind: "file", message: "Cap the push body below the memory cliff", when: "2 hours ago" },
    { name: "main.rs", kind: "file", message: "Install the ring CryptoProvider before anything opens TLS", when: "4 days ago" },
    { name: "ownership.rs", kind: "file", message: "Force-claim a repo whose owner is unreachable", when: "3 days ago" },
    { name: "store.rs", kind: "file", message: "Reserve `api` as an owner name", when: "5 days ago" },
  ],
};

export const README = `# rustic

Source hosting that stores pack files in object storage and refs in an embedded
database. Every node can serve any repo; exactly one node owns a repo's writes at
a time, and ownership moves in under half a second when a node disappears.

## Layout

- \`src/http.rs\` — smart HTTP, protocol v2, the peer router
- \`src/ownership.rs\` — lease, force-claim, writer-epoch fencing
- \`src/browse.rs\` — tree / blob / log / commit for the read API
- \`src/api.rs\` — the separate api process: cache, authz, upstream

## Running

\`\`\`
cargo run -- --bucket rustic-dev --listen 0.0.0.0:8080
\`\`\`
`;

export const FILE = {
  path: "src/auth.rs",
  size: "1.9 KB",
  lines: [
    "use crate::store::Store;",
    "use crate::{err, Result};",
    "",
    "/// Whether `auth_owner` may read a repo belonging to `repo_owner`.",
    "///",
    "/// This is a function of the *resource* as much as the caller: a public repo is",
    "/// readable by anyone who is signed in, a private one only by its owner. A claim",
    "/// on a session token cannot answer it, because the token is issued before any",
    "/// repo is named.",
    "pub fn authorize(auth_owner: Option<&str>, repo_owner: &str, public_read: bool) -> bool {",
    "    public_read || auth_owner == Some(repo_owner)",
    "}",
    "",
    "#[cfg(test)]",
    "mod tests {",
    "    use super::*;",
    "",
    "    #[test]",
    "    fn public_repos_are_readable_by_other_users() {",
    "        assert!(authorize(Some(\"bob\"), \"alice\", true));",
    "    }",
    "",
    "    #[test]",
    "    fn private_repos_are_owner_only() {",
    "        assert!(!authorize(Some(\"bob\"), \"alice\", false));",
    "        assert!(authorize(Some(\"alice\"), \"alice\", false));",
    "    }",
    "}",
  ],
};

export type Commit = { sha: string; message: string; author: string; when: string; day: string; ok?: boolean };

export const COMMITS: Commit[] = [
  { sha: "15da845", message: "Cap the push body below the memory cliff", author: "alice", when: "2 hours ago", day: "Today", ok: true },
  { sha: "36f711e", message: "Do not hand a repo to a node that is leaving", author: "alice", when: "5 hours ago", day: "Today", ok: true },
  { sha: "e024dea", message: "Plan: implement object serving in five tasks", author: "alice", when: "7 hours ago", day: "Today", ok: true },
  { sha: "b7b9800", message: "Retry a release: an unreleased lease costs a full TTL", author: "karthik", when: "yesterday", day: "Yesterday", ok: true },
  { sha: "5960ae7", message: "Design: clones answered with signed URLs, not with bytes through the owner", author: "karthik", when: "yesterday", day: "Yesterday", ok: false },
  { sha: "0b772db", message: "Force-claim recovers in 0.46s instead of a full lease TTL", author: "karthik", when: "3 days ago", day: "Earlier", ok: true },
];

export const DIFF = {
  sha: "15da845",
  message: "Cap the push body below the memory cliff",
  body: "A pack larger than the node's headroom was read fully into memory before the\nwriter looked at it. Reject at the edge instead: 413 with the limit in the body.",
  author: "alice",
  when: "2 hours ago",
  parents: ["36f711e"],
  stats: { files: 2, additions: 18, deletions: 3 },
  files: [
    {
      path: "src/http.rs",
      additions: 14,
      deletions: 3,
      hunks: [
        { header: "@@ -212,9 +212,20 @@ async fn receive_pack(", lines: [
          [" ", "    let repo = state.store.open(&owner, &name).await?;"],
          ["-", "    let body = body.collect().await?.to_bytes();"],
          ["-", "    let pack = Pack::parse(&body)?;"],
          ["+", "    // A pack we cannot hold should be refused before it is read, not after."],
          ["+", "    if let Some(len) = content_length(&headers) {"],
          ["+", "        if len > MAX_PUSH_BYTES {"],
          ["+", "            return Err(err::too_large(MAX_PUSH_BYTES));"],
          ["+", "        }"],
          ["+", "    }"],
          ["+", "    let body = Limited::new(body, MAX_PUSH_BYTES).collect().await?.to_bytes();"],
          ["+", "    let pack = Pack::parse(&body)?;"],
          [" ", "    repo.ingest(pack).await?;"],
        ]},
      ],
    },
    {
      path: "src/http/limits.rs",
      additions: 4,
      deletions: 0,
      hunks: [
        { header: "@@ -0,0 +1,4 @@", lines: [
          ["+", "/// Below the point where a node's headroom runs out under two concurrent pushes."],
          ["+", "pub const MAX_PUSH_BYTES: u64 = 512 * 1024 * 1024;"],
        ]},
      ],
    },
  ],
};

export type Issue = { number: number; title: string; author: string; when: string; labels: string[]; comments: number; state: "open" | "closed" };
export const ISSUES: Issue[] = [
  { number: 42, title: "Force-claim races when two nodes lose the lead at once", author: "bob", when: "3 hours ago", labels: ["bug", "ownership"], comments: 4, state: "open" },
  { number: 41, title: "Expose blob size in tree entries so the UI can skip fetching binaries", author: "karthik", when: "yesterday", labels: ["browse-api"], comments: 1, state: "open" },
  { number: 39, title: "Protocol v0 clients get a 500 instead of a 400", author: "alice", when: "4 days ago", labels: ["bug"], comments: 6, state: "closed" },
  { number: 37, title: "Document the Redis generation counter", author: "alice", when: "last week", labels: ["docs"], comments: 0, state: "open" },
];

export type Pull = { number: number; title: string; author: string; when: string; branch: string; checks: "passing" | "failing" | "pending"; reviews: number; state: "open" | "merged" };
export const PULLS: Pull[] = [
  { number: 44, title: "Cap the push body below the memory cliff", author: "alice", when: "2 hours ago", branch: "fix/push-cap", checks: "passing", reviews: 1, state: "open" },
  { number: 43, title: "Browse API: tree, blob, log, commit on the peer router", author: "karthik", when: "yesterday", branch: "feat/browse-api", checks: "failing", reviews: 0, state: "open" },
  { number: 40, title: "Force-claim recovers in 0.46s", author: "karthik", when: "3 days ago", branch: "fix/force-claim", checks: "passing", reviews: 2, state: "merged" },
];

export type Run = { id: number; workflow: string; sha: string; branch: string; trigger: string; status: "passing" | "failing" | "running"; when: string; duration: string };
export const RUNS: Run[] = [
  { id: 318, workflow: "ci", sha: "15da845", branch: "main", trigger: "push", status: "passing", when: "2 hours ago", duration: "4m 12s" },
  { id: 317, workflow: "ci", sha: "36f711e", branch: "main", trigger: "push", status: "passing", when: "5 hours ago", duration: "4m 03s" },
  { id: 316, workflow: "ci", sha: "a1c9e77", branch: "feat/browse-api", trigger: "pull request #43", status: "failing", when: "yesterday", duration: "2m 41s" },
  { id: 315, workflow: "release", sha: "b7b9800", branch: "v1.0", trigger: "tag", status: "passing", when: "yesterday", duration: "9m 55s" },
  { id: 314, workflow: "ci", sha: "0d1e9c2", branch: "fix/force-claim", trigger: "push", status: "running", when: "just now", duration: "1m 08s" },
];

/** One pull request, opened: everything the Conversation, Commits and Files tabs
 *  need. Timeline events are typed so each renders in its own shape. */
export type TimelineEvent =
  | { kind: "comment"; id: string; author: string; when: string; body: string }
  | { kind: "review"; id: string; author: string; when: string; state: "approved" | "changes_requested" | "commented"; body?: string }
  | { kind: "commits"; id: string; author: string; when: string; commits: { sha: string; message: string }[] }
  | { kind: "checks"; id: string; when: string; status: "passing" | "failing"; detail: string }
  | { kind: "label"; id: string; author: string; when: string; label: string };

export const PULL = {
  number: 44,
  title: "Cap the push body below the memory cliff",
  state: "open" as const,
  author: "alice",
  when: "2 hours ago",
  head: "fix/push-cap",
  base: "main",
  body: `A pack larger than the node's headroom was read fully into memory before the writer looked at it. Two concurrent large pushes could take a node down.

This rejects at the edge instead — \`413\` with the limit in the body — and wraps the stream in a \`Limited\` reader so a client that lies about \`Content-Length\` is cut off at the same point.

Closes #38.`,
  commits: [
    { sha: "15da845", message: "Cap the push body below the memory cliff", author: "alice", when: "2 hours ago" },
    { sha: "c41e0b9", message: "Name the limit and put it beside the other limits", author: "alice", when: "3 hours ago" },
  ],
  stats: { files: 2, additions: 18, deletions: 3 },
  checks: [
    { name: "ci / build", status: "passing" as const, duration: "1m 12s" },
    { name: "ci / test", status: "passing" as const, duration: "2m 48s" },
    { name: "ci / clippy", status: "passing" as const, duration: "41s" },
  ],
  reviewers: [
    { login: "karthik", state: "approved" as const },
    { login: "bob", state: "pending" as const },
  ],
  labels: ["ownership", "reliability"],
  linked: [{ number: 38, title: "Node OOMs on a 1.2 GB push" }],
  timeline: [
    { kind: "commits", id: "t1", author: "alice", when: "3 hours ago", commits: [
      { sha: "c41e0b9", message: "Name the limit and put it beside the other limits" },
      { sha: "15da845", message: "Cap the push body below the memory cliff" },
    ] },
    { kind: "checks", id: "t2", when: "3 hours ago", status: "passing", detail: "3 checks passed in 4m 41s" },
    { kind: "label", id: "t3", author: "alice", when: "3 hours ago", label: "reliability" },
    { kind: "review", id: "t4", author: "karthik", when: "1 hour ago", state: "changes_requested",
      body: "The limit is right, but the constant lives in `http.rs` next to unrelated things. Can it sit with the other limits so the next person finds it?" },
    { kind: "comment", id: "t5", author: "alice", when: "50 minutes ago", body: "Moved to `http/limits.rs` and pushed. Same value." },
    { kind: "review", id: "t6", author: "karthik", when: "20 minutes ago", state: "approved", body: "Thanks — that reads clearly now." },
  ] as TimelineEvent[],
};

/** Every path in the repo, for "Go to file". The API will return this as one flat
 *  list per ref; building it from TREE keeps the mock honest with itself. */
export const PATHS: { path: string; kind: "dir" | "file" }[] = Object.entries(TREE).flatMap(([dir, entries]) =>
  entries.map((e) => ({ path: dir ? `${dir}/${e.name}` : e.name, kind: e.kind })),
);
