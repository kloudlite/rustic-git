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

/** Which tools never ask: reads and messages that do not change platform state. */
const UNGATED = new Set(["ask", "plan", "skill", "tool_search", "memory", "question", "report", "kl_pkg_list"]);

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

  { name: "report", group: "workspace", summary: "Report on an ask you are holding: `progress` is your decision or a milestone and does not answer it, `done` or `blocked` answers it. The first report says what you are going ahead with.", effect: "read" },

  { name: "kl_workspace_progress", group: "workspace", summary: "What a workspace's session is doing: what has been asked of it, and the last of what it said.", effect: "read" },

  { name: "kl_pkg_list", group: "workspace", summary: "The packages a workspace has, and whether they are ready.", effect: "read" },
  { name: "kl_pkg_add", group: "workspace", summary: "Add packages to a workspace — nixpkgs attributes (rustc, cargo, nodejs_22, go, python3), not language names; `attr@version` pins. Every other installed package stays as it is.", effect: "write" },
  { name: "kl_pkg_rm", group: "workspace", summary: "Remove packages from a workspace. Every other installed package stays as it is.", effect: "write" },

  { name: "kl_repos", group: "code", summary: "Repositories you can see — yours, or an owner's with `owner`.", effect: "read" },
  { name: "kl_repo_create", group: "code", summary: "Create a repository under you or a team.", effect: "write", ask: (a) => `Create repository ${a.owner ?? "you"}/${a.name}${a.visibility ? ` (${a.visibility})` : ""}` },
  { name: "kl_repo_branches", group: "code", summary: "A repository's branches, and which is the default.", effect: "read" },
  { name: "kl_repo_clone", group: "code", summary: "Clone a repository into this machine over ssh, with the person's own key.", effect: "write" },
  { name: "kl_pulls", group: "code", summary: "Pull requests on a repository (`state` to narrow).", effect: "read" },
  { name: "kl_pull", group: "code", summary: "One pull request in full.", effect: "read" },
  { name: "kl_pull_create", group: "code", summary: "Open a pull request from head into base.", effect: "write", ask: (a) => `Open a pull request on ${a.repo}: ${a.head} → ${a.base} — ${a.title}` },
  { name: "kl_pull_merge", group: "code", summary: "Merge a pull request (fast-forward, squash, merge or rebase).", effect: "write", ask: (a) => `Merge ${a.repo}#${a.number} (${a.method ?? "fast-forward"})` },
  { name: "kl_pull_close", group: "code", summary: "Close a pull request without merging it.", effect: "write", ask: (a) => `Close ${a.repo}#${a.number} without merging` },

  { name: "kl_container_build", group: "code", summary: "Build an image from a context in a workspace and push it. In that workspace it runs there; from the bench it asks the workspace to do it, since the bench has no tree of its own.", effect: "write" },
  { name: "kl_container_push", group: "code", summary: "Copy an image the registry already holds to another tag. From the bench it asks a workspace to run it.", effect: "write", ask: (a) => `Copy image ${a.from} to ${a.to}` },
  { name: "kl_images", group: "code", summary: "Images in the registry, by owner. A registry read: no machine is involved.", effect: "read" },

  { name: "kl_workspaces", group: "workspace", summary: "List workspaces — yours, or a team's with `team`.", effect: "read" },
  { name: "kl_workspace", group: "workspace", summary: "One workspace in full: state, node, packages, its space's environment.", effect: "read" },
  { name: "kl_workspace_create", group: "workspace", summary: "Create a workspace — empty, from a repo and branch, or from a snapshot, never two of those in one call. `packages` are nixpkgs attributes (rustc, cargo, nodejs_22, go, python3), never language names.", effect: "write", ask: (a) => `Create workspace ${a.name}${a.repo ? ` from ${a.repo}${a.branch ? `#${a.branch}` : ""}` : a.from_snapshot ? ` from snapshot ${a.from_snapshot}` : ""}${withList("", a.packages)}` },
  { name: "kl_workspace_start", group: "workspace", summary: "Start a stopped workspace.", effect: "write", ask: (a) => `Start workspace ${a.id}` },
  { name: "kl_workspace_stop", group: "workspace", summary: "Stop a running workspace (cuts a sync point first).", effect: "write", ask: (a) => `Stop workspace ${a.id}` },
  { name: "kl_workspace_snapshot", group: "workspace", summary: "Take a snapshot of a workspace, with a message.", effect: "write", ask: (a) => `Snapshot workspace ${a.id}${a.message ? `: ${a.message}` : ""}` },
  { name: "kl_workspace_snapshots", group: "workspace", summary: "A workspace's snapshots: what they say and when they were taken.", effect: "read" },
  { name: "kl_workspace_clone", group: "workspace", summary: "Clone a workspace into a new one from its latest sync point.", effect: "write", ask: (a) => `Clone workspace ${a.id} into ${a.name}` },
  { name: "kl_workspace_delete", group: "workspace", summary: "Delete a workspace; its snapshots survive on the volume.", effect: "destroy", ask: (a) => `Delete workspace ${a.id}; its snapshots stay on the volume` },

  { name: "kl_env_current", group: "environment", summary: "Which environment each of your spaces uses. The environment is chosen for the whole SPACE (the team), not per workspace.", effect: "read" },
  { name: "kl_env_switch", group: "environment", summary: "Choose the environment for the whole SPACE (the team): every workspace in it then resolves that environment's services by bare name. There is no per-workspace attach, so this is what \"attach this workspace to that environment\" means.", effect: "write", ask: (a) => `Use environment ${a.environment} for this space` },
  { name: "kl_env_clear", group: "environment", summary: "Stop using an environment in this space — for every workspace in it, since the choice is the space's and not one workspace's.", effect: "write", ask: () => `Stop using an environment in this space` },
  { name: "kl_environments", group: "environment", summary: "List environments you can see.", effect: "read" },
  { name: "kl_environment", group: "environment", summary: "One environment in full: services, ports, intercepts.", effect: "read" },
  { name: "kl_environment_create", group: "environment", summary: "Create a NEW environment from a services list, or from a snapshot.", effect: "write", ask: (a) => `Create environment ${a.name}${a.from_snapshot ? ` from snapshot ${a.from_snapshot}` : ` with ${(a.services ?? []).map((s: any) => s.name).join(", ") || "no services"}`}` },
  { name: "kl_environment_start", group: "environment", summary: "Start a stopped environment.", effect: "write", ask: (a) => `Start environment ${a.id}` },
  { name: "kl_environment_stop", group: "environment", summary: "Stop an environment.", effect: "write", ask: (a) => `Stop environment ${a.id}` },
  { name: "kl_environment_snapshot", group: "environment", summary: "Take a snapshot of an environment, with a message.", effect: "write", ask: (a) => `Snapshot environment ${a.id}${a.message ? `: ${a.message}` : ""}` },
  { name: "kl_environment_snapshots", group: "environment", summary: "An environment's snapshots: what they say and when they were taken.", effect: "read" },
  { name: "kl_environment_clone", group: "environment", summary: "Clone an environment into a new one.", effect: "write", ask: (a) => `Clone environment ${a.id} into ${a.name}` },
  { name: "kl_intercept", group: "environment", summary: "Deliver a service's traffic to a workspace, or clear it with explicit null. Omission is refused; an omitted port map forwards every port one to one. A port remap is {service, workspace}: the port callers already dial, and the port the workspace listens on.", effect: "write", ask: (a) => (a.workspace === null ? `Stop intercepting ${a.service} in ${a.id}` : `Deliver ${a.service} traffic in ${a.id} to workspace ${a.workspace}${a.ports?.length ? ` (${a.ports.map((p: any) => `${p.service}→${p.workspace}`).join(", ")})` : ""}`) },
  { name: "kl_environment_service_add", group: "environment", summary: "Add a service to an environment, or replace one of the same name whole; every other service is kept as it is, and a replaced service does not inherit the fields you leave out.", effect: "write", ask: (a) => `Add service ${a.service?.name} (${a.service?.image}) to environment ${a.id}` },
  { name: "kl_environment_service_rm", group: "environment", summary: "Remove a service from an environment; its workload goes, its files stay on the volume.", effect: "destroy", ask: (a) => `Remove service ${a.name} from environment ${a.id}; its files stay on the volume` },
  { name: "kl_environment_restore", group: "environment", summary: "Put an environment back to one of its own snapshots, in place. It does not create an environment.", effect: "write", ask: (a) => `Restore snapshot ${a.snapshot} into environment ${a.id}, in place` },
  { name: "kl_environment_delete", group: "environment", summary: "Delete an environment.", effect: "destroy", ask: (a) => `Delete environment ${a.id}` },

  { name: "ask_close", group: "workspace", summary: "Close an agent, its transcript, and the working directory it was given.", effect: "write", ask: (a) => `Close agent ${a.name} and delete its working directory` },
  { name: "plan", group: "workspace", summary: "Write the plan for this work, and tick steps as they land.", effect: "read" },
  { name: "skill", group: "workspace", summary: "What a part of the platform is and the verbs it has: workspaces, environments, snapshots, repos, images, agents.", effect: "read" },
  { name: "tool_search", group: "workspace", summary: "Find the tool for a platform verb, by what you want to do. It answers the names and parameters, and turns them on.", effect: "read" },

  { name: "question", group: "workspace", summary: "Ask the person to CHOOSE between real alternatives you cannot decide. Never to confirm an action — call the tool and the harness asks for you. Read `architecture` and `memory` first; if either answers it, do not ask.", effect: "read" },
  { name: "memory", group: "workspace", summary: "Remember something the person told you, or forget one that is no longer true.", effect: "read" },
  { name: "architecture", group: "workspace", summary: "What runs where and what talks to what, for this space; `set` replaces one section.", effect: "read" },

  { name: "kl_capabilities", group: "platform", summary: "Everything you can do here, by name and effect. Read this instead of going looking.", effect: "read" },
];
