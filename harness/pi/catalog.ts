/**
 * Every tool the bench can call, described once: the extension registers from
 * this list and the Settings › Tools page renders it, so what the model sees
 * and what a person reads are the same catalogue. A bench session has no hands
 * in its own pod — no builtin tools, no shell — so nothing here is local:
 * `kl_*` is /v1, `kl_pkg_*`/`kl_env_*` act on the machine it is itself, and
 * another workspace is only ever ASKED (`kl_workspace_ask`), never driven.
 */
export type ToolSpec = {
  name: string;
  group: "workspace" | "environment" | "platform" | "code";
  summary: string;
  /** What the call does to the platform; a person decides on this. */
  effect: "read" | "write" | "destroy";
  /**
   * The question a person is asked before this runs, in their terms — "Create workspace
   * svelte-backend in centralindia-k3s with nodejs, go", not the tool name and a blob of JSON.
   * Only a gated tool needs one; `question()` falls back to a readable default.
   */
  ask?: (a: Record<string, any>) => string;
};

/** Which tools never ask: a message to another session, and this machine's own packages. */
const UNGATED = new Set(["ask", "ask_close", "plan", "skill", "tool_search", "memory", "kl_pkg_list", "kl_pkg_add", "kl_pkg_rm"]);

/** Whether a call has to be asked about first: it changes somebody's platform state (spec §9). */
export function gated(name: string): boolean {
  const t = TOOLS.find((x) => x.name === name);
  return !!t && (t.effect === "write" || t.effect === "destroy") && !UNGATED.has(name);
}

/** The one line the person is asked. */
export function question(name: string, a: Record<string, any>): string {
  const t = TOOLS.find((x) => x.name === name);
  if (t?.ask) return t.ask(a);
  const subject = a.id ?? a.name ?? a.workspace ?? a.service ?? "";
  return `${(t?.name ?? name).replace(/^kl_/, "").replace(/_/g, " ")}${subject ? ` ${subject}` : ""}`;
}

const list = (v: unknown) => (Array.isArray(v) && v.length ? v.join(", ") : "");
const withList = (label: string, v: unknown) => (list(v) ? ` with ${label}${list(v)}` : "");

export const TOOLS: ToolSpec[] = [

  { name: "ask", group: "workspace", summary: "Ask a workspace's own session to do something, or start a fresh agent with one task.", effect: "write" },

  { name: "kl_workspace_progress", group: "workspace", summary: "What a workspace's session is doing: what has been asked of it, and the last of what it said.", effect: "read" },

  { name: "kl_pkg_list", group: "workspace", summary: "The packages this machine has, and whether they are ready.", effect: "read" },
  { name: "kl_pkg_add", group: "workspace", summary: "Add packages to this machine (`attr` or `attr@version`).", effect: "write" },
  { name: "kl_pkg_rm", group: "workspace", summary: "Remove packages from this machine.", effect: "write" },

  { name: "kl_repos", group: "code", summary: "Repositories you can see — yours, or an owner's with `owner`.", effect: "read" },
  { name: "kl_repo_create", group: "code", summary: "Create a repository under you or a team.", effect: "write", ask: (a) => `Create repository ${a.owner ?? "you"}/${a.name}${a.visibility ? ` (${a.visibility})` : ""}` },
  { name: "kl_repo_branches", group: "code", summary: "A repository's branches, and which is the default.", effect: "read" },
  { name: "kl_repo_clone", group: "code", summary: "Clone a repository into this machine over ssh, with the person's own key.", effect: "write" },
  { name: "kl_pulls", group: "code", summary: "Pull requests on a repository (`state` to narrow).", effect: "read" },
  { name: "kl_pull", group: "code", summary: "One pull request in full.", effect: "read" },
  { name: "kl_pull_create", group: "code", summary: "Open a pull request from head into base.", effect: "write", ask: (a) => `Open a pull request on ${a.repo}: ${a.head} → ${a.base} — ${a.title}` },
  { name: "kl_pull_merge", group: "code", summary: "Merge a pull request (fast-forward, squash, merge or rebase).", effect: "write", ask: (a) => `Merge ${a.repo}#${a.number} (${a.method ?? "fast-forward"})` },
  { name: "kl_pull_close", group: "code", summary: "Close a pull request without merging it.", effect: "write", ask: (a) => `Close ${a.repo}#${a.number} without merging` },

  { name: "kl_container_build", group: "code", summary: "Build an image from a context in this machine and push it, as a background process.", effect: "write" },
  { name: "kl_container_push", group: "code", summary: "Copy an image the registry already holds to another tag.", effect: "write", ask: (a) => `Copy image ${a.from} to ${a.to}` },
  { name: "kl_images", group: "code", summary: "Images in the registry, by owner.", effect: "read" },

  { name: "kl_workspaces", group: "workspace", summary: "List workspaces — yours, or a team's with `team`.", effect: "read" },
  { name: "kl_workspace", group: "workspace", summary: "One workspace in full: state, node, packages, its space's environment.", effect: "read" },
  { name: "kl_workspace_create", group: "workspace", summary: "Create a workspace — empty, from a repo and branch, or from a snapshot.", effect: "write", ask: (a) => `Create workspace ${a.name}${a.repo ? ` from ${a.repo}${a.branch ? `#${a.branch}` : ""}` : ""}${withList("", a.packages)}` },
  { name: "kl_workspace_start", group: "workspace", summary: "Start a stopped workspace.", effect: "write", ask: (a) => `Start workspace ${a.id}` },
  { name: "kl_workspace_stop", group: "workspace", summary: "Stop a running workspace (cuts a sync point first).", effect: "write", ask: (a) => `Stop workspace ${a.id}` },
  { name: "kl_workspace_snapshot", group: "workspace", summary: "Take a snapshot of a workspace, with a message.", effect: "write", ask: (a) => `Snapshot workspace ${a.id}${a.message ? `: ${a.message}` : ""}` },
  { name: "kl_workspace_snapshots", group: "workspace", summary: "A workspace's snapshots: what they say and when they were taken.", effect: "read" },
  { name: "kl_workspace_clone", group: "workspace", summary: "Clone a workspace into a new one from its latest sync point.", effect: "write", ask: (a) => `Clone workspace ${a.id} into ${a.name}` },
  { name: "kl_workspace_delete", group: "workspace", summary: "Delete a workspace; its snapshots survive on the volume.", effect: "destroy", ask: (a) => `Delete workspace ${a.id}; its snapshots stay on the volume` },

  { name: "kl_env_current", group: "environment", summary: "Which environment this machine's space uses (and every other space of yours).", effect: "read" },
  { name: "kl_env_switch", group: "environment", summary: "Use an environment for this space — this machine and every workspace in it.", effect: "write", ask: (a) => `Use environment ${a.environment} for this space` },
  { name: "kl_env_clear", group: "environment", summary: "Stop using an environment in this space.", effect: "write", ask: () => `Stop using an environment in this space` },
  { name: "kl_environments", group: "environment", summary: "List environments you can see.", effect: "read" },
  { name: "kl_environment", group: "environment", summary: "One environment in full: services, ports, intercepts.", effect: "read" },
  { name: "kl_environment_create", group: "environment", summary: "Create an environment from a services list, or from a snapshot.", effect: "write", ask: (a) => `Create environment ${a.name} with ${(a.services ?? []).map((s: any) => s.name).join(", ") || "no services"}` },
  { name: "kl_environment_start", group: "environment", summary: "Start a stopped environment.", effect: "write", ask: (a) => `Start environment ${a.id}` },
  { name: "kl_environment_stop", group: "environment", summary: "Stop an environment.", effect: "write", ask: (a) => `Stop environment ${a.id}` },
  { name: "kl_environment_snapshot", group: "environment", summary: "Take a snapshot of an environment, with a message.", effect: "write", ask: (a) => `Snapshot environment ${a.id}${a.message ? `: ${a.message}` : ""}` },
  { name: "kl_environment_snapshots", group: "environment", summary: "An environment's snapshots: what they say and when they were taken.", effect: "read" },
  { name: "kl_environment_clone", group: "environment", summary: "Clone an environment into a new one.", effect: "write", ask: (a) => `Clone environment ${a.id} into ${a.name}` },
  { name: "kl_intercept", group: "environment", summary: "Deliver a service's traffic to a workspace (with port remap), or clear it.", effect: "write", ask: (a) => (a.workspace ? `Deliver ${a.service} traffic in ${a.id} to workspace ${a.workspace}` : `Stop intercepting ${a.service} in ${a.id}`) },
  { name: "kl_environment_service_add", group: "environment", summary: "Add a service to an environment, or replace one of the same name; every other service is kept as it is.", effect: "write", ask: (a) => `Add service ${a.service?.name} (${a.service?.image}) to environment ${a.id}` },
  { name: "kl_environment_service_rm", group: "environment", summary: "Remove a service from an environment; its workload goes, its files stay on the volume.", effect: "destroy", ask: (a) => `Remove service ${a.name} from environment ${a.id}; its files stay on the volume` },
  { name: "kl_environment_restore", group: "environment", summary: "Restore a snapshot into a new environment.", effect: "write", ask: (a) => `Restore snapshot ${a.snapshot_id} into a new environment ${a.name}` },
  { name: "kl_environment_delete", group: "environment", summary: "Delete an environment.", effect: "destroy", ask: (a) => `Delete environment ${a.id}` },

  { name: "ask_close", group: "workspace", summary: "Close an agent, its transcript, and the clone it was working in.", effect: "write" },
  { name: "plan", group: "workspace", summary: "Write the plan for this work, and tick steps as they land.", effect: "read" },
  { name: "skill", group: "workspace", summary: "What a part of the platform is and the verbs it has: workspaces, environments, snapshots, repos, images, agents.", effect: "read" },
  { name: "tool_search", group: "workspace", summary: "Find the tool for a platform verb, by what you want to do. It answers the names and parameters, and turns them on.", effect: "read" },

  { name: "memory", group: "workspace", summary: "Remember something the person told you, or forget one that is no longer true.", effect: "read" },

  { name: "kl_capabilities", group: "platform", summary: "Everything you can do here, by name and effect. Read this instead of going looking.", effect: "read" },
];
