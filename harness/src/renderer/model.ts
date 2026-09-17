// The hierarchy the harness is built around.
//
//   team
//    ├─ environment ×N        the services a team runs; a developer clones one
//    │                        to get a copy they can break
//    └─ bench                 exactly one per developer per team: where they
//        │                    work, connected to one environment at a time
//        ├─ thread ×N         the AI conversation; the first may change things,
//        │                    the rest are read-only
//        └─ workspace ×N      a working copy the developer can enter
//            └─ ephemeral ×N  a copy cut for one agent, watched not driven
//
// Placeholder data for now; the shapes are what the UI is written against.

export type FileNode = { name: string; dir?: boolean; open?: boolean; children?: FileNode[]; status?: "M" | "A" | "D" | "?" };
export type Change = { path: string; status: "M" | "A" | "D" | "?"; add: number; del: number; by?: string };

/**
 * Sample file contents and diffs, keyed by path. Real ones come from the
 * workspace's own tool server; this is enough to build the viewer against.
 */
/**
 * A changed file, whole. Each entry is the FULL file in unified form — a leading
 * space for a line that stayed, `-` for one that went, `+` for one that arrived
 * — so the viewer can show the file as it reads now with the edit marked in
 * place, rather than a hunk floating with no surroundings.
 *
 * Real ones come from the workspace's own tool server; these are enough to build
 * the viewer against.
 */
export const DIFFS: Record<string, string> = {
  "bins/agent/src/controller/run.rs": [
    "//! The agent's controllers, and what each one watches.",
    "//!",
    "//! Every controller here watches its own kind plus whatever it derives state",
    "//! from. What a controller watches decides what wakes it, so a watch that is",
    "//! too wide is not a style problem: it is load on the region's API server.",
    "",
    "use crate::{crd, Ctx};",
    "use kube::runtime::{controller::Action, predicates, watcher, Controller, WatchStreamExt};",
    "use kube::{Api, ResourceExt};",
    "use std::sync::Arc;",
    "",
    "pub async fn run(ctx: Arc<Ctx>) -> Result<()> {",
    "    let placed = watcher::Config::default().labels(&format!(\"{}={}\", crd::NODE_LABEL, ctx.node));",
    "",
    "    // Workspaces: this node's own, converged against their spec.",
    "    let workspaces = Controller::new(Api::<crd::Workspace>::all(ctx.client.clone()), placed.clone())",
    "        .owns(Api::<Pod>::all(ctx.client.clone()), Config::default())",
    "        .run(workspace::reconcile, on_error, ctx.clone());",
    "",
    "    // Owner bindings: one per owner, holding that owner's namespace quota.",
    "    let bindings = Controller::new(Api::<crd::OwnerBinding>::all(ctx.client.clone()), placed.clone())",
    "-        .watches(",
    "-            Api::<crd::Workspace>::all(ctx.client.clone()),",
    "-            placed.clone(),",
    "+        // A status write bumps no generation, and every node writes status on",
    "+        // every pass. Without this filter each of those echoes woke the owner's",
    "+        // binding: two quota GETs and three applies for nothing (2026-09-12).",
    "+        .watches_stream(",
    "+            watcher(Api::<crd::Workspace>::all(ctx.client.clone()), placed.clone())",
    "+                .applied_objects()",
    "+                .predicate_filter(predicates::generation, Default::default()),",
    "            {",
    "                let region = ctx.region.clone();",
    "                move |w: crd::Workspace| Some(ObjectRef::new(&crd::binding_name(&region, &w.spec.owner)))",
    "            },",
    "        )",
    "        .run(binding::reconcile, on_error, ctx.clone());",
    "",
    "    // Environments: the services a team runs, and the intercepts onto them.",
    "    //",
    "    // This watch is deliberately NOT filtered: releasing an intercept depends on",
    "    // the workspace's status — its pod's readiness — so status events are the",
    "    // whole point here.",
    "    let environments = Controller::new(Api::<crd::Environment>::all(ctx.client.clone()), placed.clone())",
    "        .watches(Api::<crd::Workspace>::all(ctx.client.clone()), placed.clone(), {",
    "            let ctx = ctx.clone();",
    "            move |w: crd::Workspace| w.spec.attached_environment.as_deref().map(ObjectRef::new)",
    "        })",
    "        .run(environment::reconcile, on_error, ctx.clone());",
    "",
    "    tokio::join!(workspaces, bindings, environments);",
    "    Ok(())",
    "}",
    "",
    "fn on_error<K>(_: Arc<K>, err: &ReconcileErr, _: Arc<Ctx>) -> Action {",
    "    tracing::warn!(error = %err, \"reconcile.failed\");",
    "    Action::requeue(Duration::from_secs(15))",
    "}",
  ].join("\n"),

  "Cargo.toml": [
    "[workspace]",
    "members = [\"bins/*\", \"crates/*\"]",
    "resolver = \"2\"",
    "",
    "[workspace.dependencies]",
    "anyhow = \"1\"",
    "axum = { version = \"0.8\", features = [\"macros\"] }",
    "kube = { version = \"2.0\", features = [\"runtime\", \"derive\"] }",
    "-kube-runtime = { version = \"4.2\", features = [\"unstable-runtime-reconcile-on\", \"unstable-runtime-subscribe\"] }",
    "+kube-runtime = { version = \"4.2\", features = [",
    "+    \"unstable-runtime-reconcile-on\",",
    "+    \"unstable-runtime-subscribe\",",
    "+    \"unstable-runtime-stream-control\",",
    "+] }",
    "k8s-openapi = { version = \"0.26\", features = [\"latest\"] }",
    "serde = { version = \"1\", features = [\"derive\"] }",
    "tokio = { version = \"1\", features = [\"full\"] }",
    "tracing = \"0.1\"",
  ].join("\n"),

  "bins/agent/tests/reconcile/binding.rs": [
    "+//! A status write must not wake the owner's binding.",
    "+//!",
    "+//! The binding controller watches Workspace so a new one gets its quota; it",
    "+//! must not wake for the status every node writes on every pass.",
    "+",
    "+use super::*;",
    "+",
    "+#[tokio::test]",
    "+async fn a_status_only_write_does_not_wake_the_binding() {",
    "+    let f = Fixture::new().await;",
    "+    f.create_workspace(\"ws-1\", \"karthik\").await;",
    "+    f.settle().await;",
    "+    let before = f.binding_passes();",
    "+",
    "+    // Same generation, new resourceVersion: exactly what a node's own status",
    "+    // write looks like from the API server.",
    "+    f.write_status(\"ws-1\", |s| s.phase = Phase::Ready).await;",
    "+    f.settle().await;",
    "+",
    "+    assert_eq!(f.binding_passes(), before, \"a status write woke the binding\");",
    "+}",
    "+",
    "+#[tokio::test]",
    "+async fn a_spec_change_still_wakes_it() {",
    "+    let f = Fixture::new().await;",
    "+    f.create_workspace(\"ws-1\", \"karthik\").await;",
    "+    f.settle().await;",
    "+    let before = f.binding_passes();",
    "+",
    "+    f.patch_spec(\"ws-1\", |s| s.desired_state = DesiredState::Stopped).await;",
    "+    f.settle().await;",
    "+",
    "+    assert!(f.binding_passes() > before, \"a spec change did not wake the binding\");",
    "+}",
  ].join("\n"),
};

export const FILES: Record<string, string> = {
  "bins/agent/src/controller/status.rs": `//! Status writes, and the one rule that governs them: a controller writes
//! observed state only, through the /status subresource, and never the spec.

use super::*;

pub(crate) async fn write<K>(api: &Api<K>, name: &str, patch: Value) -> Result<(), ReconcileErr> {
    api.patch_status(name, &PatchParams::apply(FIELD_MANAGER), &Patch::Merge(patch))
        .await
        .map(|_| ())
        .map_err(ReconcileErr::from)
}`,
};

export type AgentState = "running" | "waiting" | "done" | "failed" | "idle";

// Nobody drives an ephemeral. An agent is given a task, works in its copy, and
// folds the result back into the workspace it was cut from itself — there is no
// review step and no request for one. Everything about one in this app is
// therefore read-only: watching progress, not steering it.
export type Ephemeral = {
  id: string;
  task: string;
  agent: string;
  state: AgentState;
  started: string;
  step?: string;                             // what it is doing right now
  outcome?: { summary: string; at: string };  // what became of the work
  files?: FileNode[];                         // defaults to the workspace's tree
  changes: Change[];
};

/** A package as the platform pins it: `attr@version`, or bare for the region's nixpkgs pin. */
export type Package = { name: string; version: string; pinned?: boolean };

/**
 * One message between the machine and a workspace: the machine hands a
 * workspace something to do (`in`), the workspace reports back (`out`). A
 * workspace's overview shows this queue, so what the machine asked of it and
 * what it answered can be read without opening either thread.
 */
export type Exchange = { dir: "in" | "out"; at: string; text: string; state: "pending" | "working" | "done"; session: string };

export type Workspace = {
  id: string;
  queue: Exchange[];
  name: string;
  repo: string;
  branch: string;
  state: "running" | "stopped";
  packages: Package[];
  ephemerals: Ephemeral[];
  files: FileNode[];
  changes: Change[];
};

export type ServiceState = "running" | "starting" | "stopped" | "failed";

// A port says what it speaks, because that is what decides whether a person can
// open it: an http port is a page, anything else is an address to dial.
//
// An intercept lives on the port, not the service: the environment keeps sending
// traffic to the same address, and this one port is delivered to a workspace
// instead — often on a different number, since a dev server listens where a dev
// server listens.
export type Port = {
  port: number;
  protocol: "http" | "https" | "tcp" | "grpc";
  url?: string;
  intercept?: { workspace: string; port: number };
};
export type Service = { name: string; image: string; state: ServiceState; ports: Port[]; note?: string };

// An environment belongs to the TEAM, not to any one machine. A machine connects
// to exactly one at a time.
//
// A developer gets one of their own three ways: empty, cloned from an
// environment that is running, or cloned from a SNAPSHOT — a frozen copy of an
// environment taken at a moment, which is also how a known-good state is kept
// before something risky. `from` records which, so the list can say where an
// environment came from without a second lookup.
export type Environment = {
  id: string;
  name: string;
  owner: "team" | "you";
  from?: { kind: "environment" | "snapshot"; name: string };
  region: string;
  volume?: string;   // the volume its snapshot history is read from; absent until something was pushed
  services: Service[];
};

/** A frozen copy of an environment: what it ran and how it was configured. */
export type Snapshot = {
  id: string;
  name: string;
  environment: string;
  at: string;
  by: string;
  services: number;
  note?: string;
};

/** A thread belongs to a node of the machine's tree: the machine's own is the
    one that changes things, a workspace's is where that copy is worked, an
    ephemeral's is a log to watch — nobody drives one. */
export type Thread = { id: string; name: string; kind: "machine" | "session" | "workspace" | "ephemeral" | "btw"; readonly: boolean; messages: Message[]; pi?: string; session?: string; model?: string };

export type TodoState = "done" | "active" | "blocked" | "pending";
// A plan is a tree: a step may hold sub-steps. A parent's state is derived from
// its children (done when all are, active when any is), never stored.
export type Todo = { id: string; text: string; state?: TodoState; eph?: string; note?: string; children?: Todo[] };

/** A repository the team has on the platform; a workspace is cut from one. */
export type Repo = { id: string; teamId: string; name: string; branch: string; updated: string; private?: boolean };
export const REPOS: Repo[] = [
  { id: "r-rustic-git", teamId: "t-kloudlite", name: "kloudlite/rustic-git", branch: "master", updated: "1h ago", private: true },
  { id: "r-harness", teamId: "t-kloudlite", name: "kloudlite/harness", branch: "main", updated: "3h ago", private: true },
  { id: "r-infra", teamId: "t-kloudlite", name: "kloudlite/infra", branch: "main", updated: "2d ago", private: true },
  { id: "r-docs", teamId: "t-kloudlite", name: "kloudlite/docs", branch: "main", updated: "1w ago" },
  { id: "r-labs", teamId: "t-labs", name: "labs/playground", branch: "main", updated: "4d ago" },
];

/** An image in the team's registry, with the tags it carries. */
export type Image = { id: string; teamId: string; name: string; tags: string[]; pushed: string; size: string; pulls: number };
export const IMAGES: Image[] = [
  { id: "i-api", teamId: "t-kloudlite", name: "kloudlite/api", tags: ["bc5a5062", "925536aa", "latest"], pushed: "2h ago", size: "84 MB", pulls: 1412 },
  { id: "i-server", teamId: "t-kloudlite", name: "kloudlite/server", tags: ["bc5a5062", "latest"], pushed: "2h ago", size: "91 MB", pulls: 1398 },
  { id: "i-agent", teamId: "t-kloudlite", name: "kloudlite/agent", tags: ["bc5a5062", "latest"], pushed: "2h ago", size: "77 MB", pulls: 620 },
  { id: "i-web", teamId: "t-kloudlite", name: "kloudlite/web", tags: ["925536aa", "latest"], pushed: "1d ago", size: "212 MB", pulls: 388 },
  { id: "i-slo", teamId: "t-kloudlite", name: "kloudlite/slo", tags: ["bc5a5062"], pushed: "2h ago", size: "63 MB", pulls: 96 },
  { id: "i-labs", teamId: "t-labs", name: "labs/hello", tags: ["1", "latest"], pushed: "4d ago", size: "12 MB", pulls: 3 },
];

/**
 * Where a model comes from. A provider is connected by a key the person adds
 * once per machine; the machine's `model` is one of a connected provider's.
 * A local provider needs no key — it is connected when the process answers.
 */
export type Provider = {
  id: string;
  name: string;
  state: "connected" | "no-key" | "unreachable";
  models: string[];
  note?: string;
};

export const PROVIDERS: Provider[] = [
  { id: "anthropic", name: "Anthropic", state: "connected", models: ["claude-fable-5-1", "claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"] },
  { id: "openai", name: "OpenAI", state: "connected", models: ["gpt-5.2", "gpt-5.2-mini", "o4"] },
  { id: "deepseek", name: "DeepSeek", state: "connected", models: ["deepseek-v4", "deepseek-r2"] },
  { id: "google", name: "Google", state: "no-key", models: ["gemini-3-pro", "gemini-3-flash"] },
  { id: "ollama", name: "Ollama", state: "unreachable", models: ["qwen3-coder:32b", "llama4:70b"], note: "nothing listening on localhost:11434" },
  { id: "openrouter", name: "OpenRouter", state: "no-key", models: [] },
];

/**
 * What extends the machine. A SKILL is a prompt the machine can be told to
 * follow (`/name` in the composer); an MCP SERVER is a process that lends it
 * tools; a HOOK runs at a moment of the machine's own loop. All three are the
 * machine's, not a workspace's: an agent cut from it inherits them.
 */
export type Plugin =
  | { kind: "skill"; name: string; from: string; enabled: boolean; summary: string }
  | { kind: "mcp"; name: string; from: string; enabled: boolean; state: "connected" | "starting" | "failed" | "off"; tools: number; note?: string }
  | { kind: "hook"; name: string; from: string; enabled: boolean; on: string };

// Exactly one per developer per team. Switching teams therefore switches the
// machine; there is no machine to choose within a team.
export type Machine = {
  id: string;
  teamId: string;
  owner: string;
  goal: string;
  model: string;
  environmentId: string;
  todos: Todo[];
  workspaces: Workspace[];
  plugins: Plugin[];
};

export type Message =
  /** `local`: echoed the moment it was typed, before pi reported taking it; its time is not pi's yet. */
  | { role: "user"; text: string; at: string; ts?: number; images?: number[]; local?: true }
  | { role: "assistant"; text: string; at: string; ts?: number }
  /** A question the harness is waiting on: nothing changes until the person answers it (spec §9). */
  | { role: "question"; id: string; tool: string; summary: string; args?: Record<string, unknown>; answer?: "yes" | "no"; at: string; ts?: number }
  | { role: "action"; kind: "spawn" | "run" | "fold" | "note"; text: string; target?: string; at: string; ts?: number; ok?: boolean; output?: string; pending?: boolean; tool?: string; args?: Record<string, unknown>; ms?: number };

export const MACHINE: Machine = {
  id: "wm-karthik-kloudlite",
  teamId: "t-kloudlite",
  owner: "karthik@kloudlite.io",
  goal: "Stop the region API churn: a workspace status write must not wake its owner's binding; verify on the fleet.",
  model: "claude-fable-5-1",
  environmentId: "env-1dfa74",
  plugins: [
    { kind: "skill", name: "ship", from: "kloudlite/deploy", enabled: true, summary: "pin, roll and verify a build on the fleet" },
    { kind: "skill", name: "review", from: "kloudlite/deploy", enabled: true, summary: "review a diff against the plan it came from" },
    { kind: "skill", name: "brainstorm", from: "superpowers", enabled: false, summary: "turn an idea into a design before any code" },
    { kind: "mcp", name: "graft", from: "graft", enabled: true, state: "connected", tools: 5 },
    { kind: "mcp", name: "clickstack", from: "clickstack", enabled: true, state: "connected", tools: 29 },
    { kind: "mcp", name: "chrome", from: "claude-in-chrome", enabled: true, state: "failed", tools: 0, note: "extension not running" },
    { kind: "mcp", name: "figma", from: "figma", enabled: false, state: "off", tools: 0 },
    { kind: "hook", name: "pod-only", from: "kloudlite/deploy", enabled: true, on: "before a shell command" },
    { kind: "hook", name: "no-attribution", from: "kloudlite/deploy", enabled: true, on: "before a commit" },
  ],
  todos: [
    { id: "t1", text: "Diagnose the churn", children: [
      { id: "t1a", text: "Rule out the datastore", state: "done", note: "kine healthy: 138 MB WAL, 150 ms compactions" },
      { id: "t1b", text: "Find why quota GETs stay at ~50/min with no leaked namespaces", state: "done", note: "OwnerBinding .watches(Workspace) fires on every status write" },
    ]},
    { id: "t2", text: "Fix the OwnerBinding watch", children: [
      { id: "t2a", text: "Filter the watch by generation", state: "done", eph: "eph-a1" },
      { id: "t2b", text: "Tests and clippy, commit, push both remotes", state: "done", note: "bc5a5062" },
      { id: "t2c", text: "Ship, pin, roll AKS and k3s", state: "done", note: "pin 2f63b877" },
    ]},
    { id: "t3", text: "Prove it", children: [
      { id: "t3a", text: "Add a reconcile test for a status-only write", state: "active", eph: "eph-a2" },
      { id: "t3b", text: "Review bc5a5062 against the plan", state: "blocked", eph: "eph-a3", note: "waits on the test" },
      { id: "t3c", text: "Measure on the fleet", children: [
        { id: "t3c1", text: "Binding passes per 5 min before and after", state: "pending" },
        { id: "t3c2", text: "Quota GET counter delta", state: "pending" },
        { id: "t3c3", text: "Hourly probe on the new build", state: "pending" },
      ]},
    ]},
    { id: "t4", text: "Close out", children: [
      { id: "t4a", text: "Record the root cause in memory", state: "pending" },
      { id: "t4b", text: "Close phase G on the board", state: "pending" },
    ]},
  ],
  workspaces: [
    {
      id: "ws-51480ba5",
      queue: [
        { session: "bench", dir: "in", at: "12:04", text: "Filter the OwnerBinding watch by generation; keep the status-only path out of the binding pass.", state: "done" },
        { session: "bench", dir: "out", at: "12:09", text: "Folded 2 files into master as bc5a5062. 5 tests pass, clippy clean.", state: "done" },
        { session: "bench", dir: "in", at: "12:41", text: "Add a reconcile test that proves a status-only write no longer wakes the binding.", state: "working" },
        { session: "bench", dir: "in", at: "12:44", text: "Review bc5a5062 against the plan once the test lands.", state: "pending" },
      ],
      packages: [{ name: "rustc", version: "1.89.0", pinned: true }, { name: "cargo", version: "1.89.0", pinned: true }, { name: "clippy", version: "1.89.0" }, { name: "kubectl", version: "1.33.2" }, { name: "jq", version: "1.7.1" }, { name: "ripgrep", version: "14.1.1" }],
      name: "rustic-git",
      repo: "kloudlite/rustic-git",
      branch: "master",
          state: "running",
      files: [
        { name: "bins", dir: true, open: true, children: [
          { name: "agent", dir: true, open: true, children: [
            { name: "src", dir: true, open: true, children: [
              { name: "controller", dir: true, open: true, children: [
                { name: "run.rs", status: "M" }, { name: "workspace", dir: true }, { name: "environment", dir: true }, { name: "status.rs" },
              ]},
              { name: "peer", dir: true }, { name: "claim.rs" }, { name: "lib.rs" }, { name: "sync.rs" },
            ]},
            { name: "tests", dir: true, open: true, children: [
              { name: "reconcile", dir: true, open: true, children: [{ name: "binding.rs", status: "A" }, { name: "mod.rs" }] },
            ]},
          ]},
          { name: "api", dir: true }, { name: "server", dir: true }, { name: "slo", dir: true },
        ]},
        { name: "crates", dir: true },
        { name: "deploy", dir: true },
        { name: "Cargo.toml", status: "M" },
        { name: "CLAUDE.md" },
      ],
      changes: [
        { path: "bins/agent/src/controller/run.rs", status: "M", add: 11, del: 2, by: "eph-a1" },
        { path: "Cargo.toml", status: "M", add: 1, del: 1, by: "eph-a1" },
        { path: "bins/agent/tests/reconcile/binding.rs", status: "A", add: 38, del: 0, by: "eph-a2" },
      ],
      ephemerals: [
        { id: "eph-a1", task: "Filter the OwnerBinding watch by generation", agent: "implementer", state: "done", started: "12:04", outcome: { summary: "folded into master as bc5a5062", at: "12:09" }, changes: [
          { path: "bins/agent/src/controller/run.rs", status: "M", add: 11, del: 2 },
          { path: "Cargo.toml", status: "M", add: 1, del: 1 },
        ] },
        { id: "eph-a2", task: "Add a reconcile test for a status-only write", agent: "implementer", state: "running", started: "12:41", step: "running the agent test suite", changes: [
          { path: "bins/agent/tests/reconcile/binding.rs", status: "A", add: 38, del: 0 },
        ] },
        { id: "eph-a3", task: "Review bc5a5062 against the plan", agent: "reviewer", state: "waiting", started: "12:44", step: "waiting on eph-a2 to finish", changes: [] },
      ],
    },
    {
      id: "ws-ea918209",
      queue: [
        { session: "bench", dir: "in", at: "11:52", text: "Wire the fleet build KPI to /admin/overview from the history series.", state: "working" },
        { session: "bench", dir: "out", at: "12:31", text: "Typecheck failed on adminSeries typing; retrying with the narrowed union.", state: "working" },
      ],
      packages: [{ name: "bun", version: "1.2.19", pinned: true }, { name: "nodejs", version: "22.17.0", pinned: true }, { name: "ripgrep", version: "14.1.1" }],
      name: "web",
      repo: "kloudlite/rustic-git",
      branch: "web/superadmin-kpis",
          state: "running",
      files: [
        { name: "web", dir: true, open: true, children: [
          { name: "apps", dir: true, open: true, children: [
            { name: "web", dir: true, open: true, children: [
              { name: "src", dir: true, open: true, children: [
                { name: "app", dir: true, open: true, children: [{ name: "superadmin", dir: true, open: true, children: [{ name: "page.tsx", status: "M" }] }] },
                { name: "components", dir: true }, { name: "lib", dir: true, open: true, children: [{ name: "api", dir: true, open: true, children: [{ name: "admin.ts", status: "M" }] }] },
              ]},
            ]},
          ]},
          { name: "package.json" },
        ]},
      ],
      changes: [
        { path: "web/apps/web/src/app/superadmin/page.tsx", status: "M", add: 22, del: 4, by: "eph-b1" },
        { path: "web/apps/web/src/lib/api/admin.ts", status: "M", add: 9, del: 0, by: "eph-b1" },
        { path: "web/apps/web/src/components/superadmin/ui/build-tile.tsx", status: "A", add: 41, del: 0, by: "eph-b1" },
      ],
      ephemerals: [
        { id: "eph-b1", task: "Wire fleet build KPI to /admin/overview", agent: "implementer", state: "failed", started: "11:52", step: "typecheck failed; retrying", changes: [
          { path: "web/apps/web/src/app/superadmin/page.tsx", status: "M", add: 22, del: 4 },
          { path: "web/apps/web/src/lib/api/admin.ts", status: "M", add: 9, del: 0 },
          { path: "web/apps/web/src/components/superadmin/ui/build-tile.tsx", status: "A", add: 41, del: 0 },
        ] },
      ],
    },
    {
      id: "ws-0c77d1e2",
      queue: [],
      packages: [{ name: "nodejs", version: "22.17.0", pinned: true }],
      name: "harness",
      repo: "kloudlite/harness",
      branch: "main",
          state: "stopped",
      files: [{ name: "src", dir: true }, { name: "package.json" }, { name: "vite.config.ts" }],
      changes: [],
      ephemerals: [],
    },
  ],
};

export const TRANSCRIPT: Message[] = [
  { role: "user", at: "12:02", text: "is it issue of reconciler that is making so many calls?" },
  { role: "assistant", at: "12:03", text: "Yes. The OwnerBinding controller watches Workspace and maps every status write to a binding pass. Each pass costs two Quota GETs and three applies; 114 passes in ten minutes for six bindings at one unchanged resourceVersion." },
  { role: "action", kind: "spawn", at: "12:04", target: "rustic-git › eph-a1", text: "Started an implementer: filter the OwnerBinding watch by generation" },
  { role: "action", kind: "run", at: "12:08", target: "eph-a1", text: "cargo test -p kloudlite-agent-bin · 5 passed", ok: true },
  { role: "action", kind: "run", at: "12:08", target: "eph-a1", text: "cargo clippy --all-targets -D warnings · clean", ok: true },
  { role: "action", kind: "fold", at: "12:09", target: "eph-a1 → rustic-git", text: "Folded 2 files into master as bc5a5062" },
  { role: "assistant", at: "12:10", text: "Committed and pushed to both remotes. Ship 14 is building; pin and roll follow, then the hourly probe and a quota GET rate check." },
  { role: "user", at: "12:40", text: "add a test that proves a status-only write no longer wakes the binding" },
  { role: "action", kind: "spawn", at: "12:41", target: "rustic-git › eph-a2", text: "Started an implementer: add a reconcile test for a status-only write" },
  { role: "action", kind: "spawn", at: "12:44", target: "rustic-git › eph-a3", text: "Started a reviewer: review bc5a5062 against the plan" },
  { role: "action", kind: "note", at: "12:45", target: "eph-a3", text: "Waiting on eph-a2 to finish before reviewing" },
];

/** Transcripts by node id; anything not here is a thread that has not started. */
const THREADS: Record<string, Message[]> = {
  "wm-karthik-kloudlite": TRANSCRIPT,
  "ws-51480ba5": [
    { role: "user", at: "12:20", text: "how do we prove the fix on the fleet without a quiet window? the fast probe fires every 5 min." },
    { role: "assistant", at: "12:21", text: "Two readings. One: count OwnerBinding reconcile.pass lines per five minutes before and after the roll; the probe does not touch bindings, so that number isolates the fix. Two: the apiserver quota GET counter, which will fall but not to zero because the probe's own creates read quota. Report both; call the second one the floor." },
    { role: "user", at: "12:22", text: "ok. and the hourly?" },
    { role: "assistant", at: "12:22", text: "Run it after the roll settles; refuse to read anything from a run that overlaps the roll. The machine has that as step 7." },
  ],
  "ws-ea918209": [
    { role: "user", at: "12:05", text: "why not just .watches with a predicate?" },
    { role: "assistant", at: "12:06", text: "Controller::watches takes a Config, not a stream, so there is nowhere to hang predicate_filter. watches_stream takes the stream you build, and that is where the generation predicate goes. It needs the unstable-runtime-stream-control feature on kube-runtime 4.2." },
  ],
  "eph-a2": [
    { role: "action", kind: "spawn", at: "12:41", target: "rustic-git › eph-a2", text: "Started an implementer: add a reconcile test for a status-only write" },
    { role: "action", kind: "run", at: "12:43", target: "eph-a2", text: "cargo test -p kloudlite-agent-bin status_only · running", },
  ],
};

/** The thread for a node of the machine, or nothing if the id names none. */
export function threadOf(m: Machine, id: string): Thread | undefined {
  const messages = THREADS[id] ?? [];
  if (id === m.id) return { id, name: "Bench Thread", kind: "machine", readonly: false, messages, pi: "bench" };
  for (const w of m.workspaces) {
    // A workspace's thread and an ephemeral's log are sessions on the bench; their messages are live, not fixtures.
    if (w.id === id) return { id, name: w.name, kind: "workspace", readonly: false, messages: [], pi: `w-${w.id}` };
    const e = w.ephemerals.find((x) => x.id === id);
    if (e) return { id, name: e.task, kind: "ephemeral", readonly: true, messages: [], pi: `e-${e.id}` };
  }
  return undefined;
}
